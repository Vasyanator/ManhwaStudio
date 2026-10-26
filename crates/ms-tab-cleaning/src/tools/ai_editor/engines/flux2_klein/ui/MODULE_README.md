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
  the derived paths of the download mode, the FLUX.2 half of the Hugging Face download
  block of 9B (heading, repository line, the token-row conditions, the encoder toggle), and
  the CATALOG download block of 4B (`draw_catalog_download_block`: «Веса трансформера» /
  «Веса текстового энкодера» as `WheelComboBox`es whose rows carry the size and a ✓ when
  installed, the derived paths, and one bundle block through the shared
  `draw_ocr_model_download`, drawn only while its snapshot matches the current bundle). The token row and the check / plan /
  download controls themselves are the shared `draw_hf_token_row` / `draw_hf_download_controls`
  (`../../shared/hf_download_ui.rs`), and so is `gated_button`, the gated-action button with
  its mandatory disabled tooltip that the component list and the prompt-cache controls use.
  The token row carries the step-by-step access hint (`ms_widgets::HfAccessHint`) naming the
  variant's model repository and, while «Расцензуренный энкодер» is in effect, the gated
  encoder repository (`Flux2VariantPresentation::uncensored_encoder_repo`).
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
  `cleaning.tools.hf_download.*` and `cleaning.tools.engine_progress.*`). A widget whose `Id` comes from a localized
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
- **The two FLUX.2 klein engines draw the SAME body**, differing where
  `Flux2PanelCtx::variant` says a capability is absent or the settings say the weight formats
  are in effect (4B in download mode: the catalog block instead of the Hugging Face one, the
  format rows of «Память и скорость», the fp8 gate; in manual mode the encoder FILE button
  where GGUF encoder formats exist). The 9B differences are OMISSIONS in `install.rs`: no Hugging Face token row for an ungated repository
  (`requires_hf_token`), no uncensored-encoder toggle where no such encoder is published
  (`supports_uncensored_encoder`). A control the backend would refuse is not drawn disabled
  — it is not drawn. The token row has ONE exception, because omitting it is otherwise a
  dead end: when the check itself blames the token (`download_check_blames_token`) the row
  comes back with a line explaining why, since `crate::hf_token` has no other UI surface
  anywhere in the application. The engine wraps the whole body in one `push_id(variant.wire())`, so
  the two instances never share a fold, a text cursor or a popup state.
- «Установка модели» is the only section built from `CollapsingState` rather than
  `RegionEditToolBase::draw_region_editor_collapsible_section`, because it must be
  openable from outside itself; the one-shot opening yields to a fold the user has moved
  by hand (`flux2_install_seed`).

## Editing map
- To move a control between sections, or to change what the panel contains: `mod.rs`.
- To change a model path row, the source-mode switch or anything about the download:
  `install.rs`.
- To change a component row: `components.rs`. To change the memory steps, the per-step rows,
  the forecast line or «Пределы»: `../../../large_model_resources/` (shared with every
  large-model engine).
- To change what a line SAYS rather than how it looks: `../decisions.rs`, with its test.
- The tests kept in this directory are CATALOG tests: they assert that every shipped
  locale carries the keys and the `{placeholder}` interpolations this panel spells, which
  a `t!` call cannot check because a unit test runs with no catalog loaded.
