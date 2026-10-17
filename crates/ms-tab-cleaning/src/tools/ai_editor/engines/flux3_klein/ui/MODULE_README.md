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
settings-file notice -> prompt (any language) -> «Референсы» (N of 9, add / remove / remove all)
-> steps + seed (blank = random; the last seed with «Повторить этот сид»)
-> the ONE readiness line («Установить» / «Обновить»)
-> «Установка модели» (install.rs) -> «Память и скорость» (shared widget) -> «Для экспертов»
   (advanced.rs) -> the mask note
```

What is touched per edit is never folded; what is set once per machine always is; no section
nests another.

## Files and submodules
- `mod.rs`: the body, the prompt, references, steps/seed, readiness and memory blocks, and the
  catalog test over every `cleaning.tools.flux3_klein.*` key (en and ru).
- `install.rs`: «Установка модели» — format picker, source-mode switch, the package folder row
  (manual) or the download block (repository + licence page, token row with
  `ms_widgets::HfAccessHint`, destination, the parts present, the shared check / download
  controls), the residency line and «Выгрузить из памяти». It opens itself once on the first
  definite non-ready verdict unless the user already moved the fold.
- `advanced.rs`: «Для экспертов» — «Референсы и в текстовый энкодер» (`vl_refs`), «Автоэнкодер в
  bfloat16», the fp8 path (drawn for fp8r only), «Ограничить референсы», soft edge, colour match.

## Contracts and invariants
- **No literals**: every text is a `cleaning.tools.flux3_klein.*` key (shared parts use
  `cleaning.tools.hf_download.*`, `large_models.*`, `cleaning.common.*`); glyph buttons carry
  their meaning on hover. Localized collapsibles and the mode buttons carry stable salts; the
  whole body sits under `push_id("flux3_klein")`.
- **Project widgets only** (`WheelSlider`, `WheelComboBox`; no `egui::Slider` / `ComboBox` /
  `DragValue`).
- **Nothing blocking**: presence marks come from the last `.status` answer, never a filesystem
  probe; file dialogs and reads are intents the engine runs on workers.
- **Decides nothing**: lines and gates come from `../decisions.rs`.

## Editing map
- Order or content of the body: `mod.rs`; installation and download: `install.rs`; expert
  switches: `advanced.rs`; what a line says: `../decisions.rs`.
