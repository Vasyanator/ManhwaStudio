# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein/ui

## Purpose
The parameter panel of the FLUX.2 klein engine — the body of the left «Редактор области»
panel and every block under it. This is the ONLY part of `flux2_klein` that draws.

## Architecture
`mod.rs` owns `Flux2PanelCtx`, the borrow of everything the panel may read or mutate for
exactly one frame, and the `draw` method whose ORDER is the design:

```
the PROMPT block (English field, the one cache line, the translator and library toggles)
-> «Сила изменения» -> the READINESS line -> three SIBLING collapsible sections
(«Установка модели» / «Память и скорость» / «Для экспертов») -> the mask note
-> «Постобработка» (the shared `draw_postprocess_section`, LAST: it docks at the bottom)
```

Two more parts are drawn OUTSIDE that scrolled body, in the sections the host pins around it:
`draw_flux2_progress_section` (progress bar + run status, top, via `AiEngine::draw_progress`)
and `draw_flux2_run_options` («Внимание только к маске», under «Обработать», via
`AiEngine::draw_run_options`). They are free functions, not `Flux2PanelCtx` methods: they need
the progress and one setting, not the whole panel borrow.

Everything a user touches per edit is above the folds; everything set once per machine is
inside them. No section wraps another and none is nested in another.

The sibling files are the blocks that body calls into. They decide nothing: what a line
says comes from `../decisions.rs`, what a `.status` answer means from `../status.rs`, and
the state they mutate belongs to `../engine/`. A control that must DO something raises a
plain flag on `Flux2PanelCtx`, which the engine folds back at the end of the frame —
never work started from inside a widget closure.

## Files and submodules
- `mod.rs`: `Flux2PanelCtx` and the panel body, plus the two pinned parts. Edit it to move a
  control between blocks or to change the order of the panel.
- `install.rs`: the source-mode switch, the editable model-path rows and their pickers,
  the derived paths of the download mode, and the CATALOG download block of both variants
  (`draw_catalog_download_block`): the «Расцензуренный энкодер» toggle where the variant offers
  that source (a switch that changes the stored encoder format leaves one line under it),
  «Веса трансформера» / «Веса текстового энкодера» as `WheelComboBox`es whose rows carry the
  real dtype or quant, the size and a ✓ when installed, the derived paths, the Hugging Face
  token row (the shared `draw_hf_token_row`) while a gated spec of the bundle is not
  installed, and one bundle block through the shared `draw_ocr_model_download` — which itself
  draws the per-repository access lines with their repository links, so this file draws no
  second list — drawn only while its snapshot matches the current bundle. `gated_button`, the
  gated-action button with its mandatory disabled tooltip that the component list and the
  prompt-cache controls use, is the shared `../../shared/hf_download_ui.rs`.
- `components.rs`: the merged
  component list — one row per component carrying presence, size, residency and the
  backend's own action buttons.
- `advanced.rs`: «Для экспертов» — the generation parameters, the growth of the mask contour
  (`mask_dilate_px`) and the precision / VAE tiling / fp8-encoder switches; the fp8 switch is
  closed, showing its effective value (off) with the reason, while a GGUF encoder format is in
  effect (D8). The feather and the
  colour match are NOT here: they are the host's post-process, drawn by the shared
  «Постобработка» section (`../../shared/postprocess_section.rs`) over
  `Flux2KleinSettings::postprocess`; its edits raise `Flux2PanelCtx::postprocess_changed` (a save,
  no forecast question), never `settings_changed`. `advanced.rs` holds the panel's only gated non-button
  control: `guidance_scale`, closed on a distilled checkpoint.
- `mask_hint.rs`: the mask hint the body closes with. The progress bar is the shared
  `draw_stream_progress`, called from `draw_flux2_progress_section` in `mod.rs`.
- «Память и скорость» has no file here: `mod.rs` `draw_memory_section` hands the shared
  `ai_editor::large_model_resources` widget the engine's forecast state for one frame
  (`ResourcesView`, from the engine's `ResourcesController`) together with the step pair and
  the persisted «Энкодер на CPU» flag, and folds its `ResourcesAction`s back through
  `Flux2PanelCtx::resources_actions`. The settings-file notice (moved aside / save blocked)
  is the first line of the body (`draw_settings_problem`).

## Contracts and invariants
- **No literal user-visible strings.** Every caption, hover and message goes through
  `t!` / `tf!` / `tp!` under `cleaning.tools.flux2_klein.*` (the download block's own
  sentences under `cleaning.tools.flux2_klein.download.*`; the shared parts it draws use
  `cleaning.tools.hf_download.*`, `cleaning.tools.engine_progress.*` and the catalog block's
  `translation.ocr_model.*`). A widget whose `Id` comes from a localized
  caption carries a stable `id_salt`, or switching language resets its state.
- **`egui::Slider`, `egui::ComboBox` and `egui::DragValue` are forbidden** in product UI;
  use the `Wheel*` widgets. See `egui-docs/04-widgets.md` §0.2.
- **Nothing blocking on the GUI thread**, filesystem probes included: a presence mark
  beside a path comes from the last `.status` answer, never from a `Path::exists` call.
- **A disabled control explains itself.** Every gated button here is disabled for a reason
  the user cannot read off the button, so the disabled tooltip is the only place that
  reason is stated — hence `gated_button` rather than a hand-threaded
  `add_enabled(..)`. The one non-button gate is `guidance_scale` in `advanced.rs`, closed
  when `.status` reports the checkpoint as distilled (`flux2_guidance_supported`): it
  carries the disabled tooltip AND a line under it, because a section the user opened
  deliberately should not need a hover to explain a grey control. Its stored value is
  untouched while it is closed.
- **A backend literal this build does not know degrades to "not known"**, with the literal
  on hover, and offers no action. It is never guessed into one of the known states.
- **The two FLUX.2 klein engines draw the SAME body**, differing only where the variant offers
  something the other does not (the uncensored-encoder toggle: `Flux2EncoderSource::offered_for`)
  and by their catalog bundle (the token row exists while a GATED spec of it is not installed:
  9B's BFL repository and the uncensored encoder; 4B's repositories are open). A control that
  could only select files nothing publishes is not drawn disabled — it is not drawn. The
  weight-format rows of «Память и скорость», the fp8 gate and the catalog block follow download
  mode for both; in manual mode the encoder FILE button is offered where GGUF encoder formats
  exist. The engine wraps the whole body in one `push_id(variant.wire())`, so the two instances
  never share a fold, a text cursor or a popup state.
- «Установка модели» is the only section built from `CollapsingState` rather than
  `RegionEditToolBase::draw_region_editor_collapsible_section`, because it must be
  openable from outside itself; the one-shot opening yields to a fold the user has moved
  by hand (`flux2_install_seed`).

## Editing map
- To move a control between sections, or to change what the panel contains: `mod.rs`.
- To change a model path row, the source-mode switch, the encoder toggle or anything about
  the download block: `install.rs`.
- To change a component row: `components.rs`. To change the memory steps, the per-step rows,
  the forecast line or «Пределы»: `../../../large_model_resources/` (shared with every
  large-model engine).
- To change what a line SAYS rather than how it looks: `../decisions.rs`, with its test.
- The tests kept in this directory are CATALOG tests: they assert that every shipped
  locale carries the keys and the `{placeholder}` interpolations this panel spells, which
  a `t!` call cannot check because a unit test runs with no catalog loaded.
