# Module: src/tabs/cleaning/tools/ai_editor/engines/flux2_klein/ui

## Purpose
The parameter panel of the FLUX.2 klein engine — the body of the left «Редактор области»
panel and every block under it. This is the ONLY part of `flux2_klein` that draws.

## Architecture
`mod.rs` owns `Flux2PanelCtx`, the borrow of everything the panel may read or mutate for
exactly one frame, and the `draw` method whose ORDER is the design:

```
progress bar -> run status -> the PROMPT block (English field, the one cache line, the
translator and library toggles) -> «Сила изменения» -> the READINESS line -> three SIBLING
collapsible sections («Установка модели» / «Память и скорость» / «Для экспертов»)
-> the mask note
```

Everything a user touches per edit is above the folds; everything set once per machine is
inside them. No section wraps another and none is nested in another.

The sibling files are the blocks that body calls into. They decide nothing: what a line
says comes from `../decisions.rs`, what a `.status` answer means from `../status.rs`, and
the state they mutate belongs to `../engine/`. A control that must DO something raises a
plain flag on `Flux2PanelCtx`, which the engine folds back at the end of the frame —
never work started from inside a widget closure.

## Files and submodules
- `mod.rs`: `Flux2PanelCtx` and the panel body. Edit it to move a control between blocks
  or to change the order of the panel.
- `install.rs`: the source-mode switch, the editable model-path rows and their pickers,
  the derived paths of the download mode, and the whole Hugging Face download block — the
  ONLY file where the two checkpoints differ on screen. Also
  `flux2_gated_button`, the gated-action button with its mandatory disabled tooltip, which
  the component list and the prompt-cache controls share.
- `components.rs`: the memory-preset picker, the RAM/VRAM forecast, and the merged
  component list — one row per component carrying presence, size, residency and the
  backend's own action buttons.
- `advanced.rs`: «Для экспертов» — the generation parameters, the mask shaping and the
  placement fields the memory preset owns.
- `progress.rs`: the progress bars and the mask hint the body opens with.

## Contracts and invariants
- **No literal user-visible strings.** Every caption, hover and message goes through
  `t!` / `tf!` / `tp!` under `cleaning.tools.flux2_klein.*` (the download block under
  `cleaning.tools.flux2_klein.download.*`). A widget whose `Id` comes from a localized
  caption carries a stable `id_salt`, or switching language resets its state.
- **`egui::Slider`, `egui::ComboBox` and `egui::DragValue` are forbidden** in product UI;
  use the `Wheel*` widgets. See `egui-docs/04-widgets.md` §0.2.
- **Nothing blocking on the GUI thread**, filesystem probes included: a presence mark
  beside a path comes from the last `.status` answer, never from a `Path::exists` call.
- **A disabled control explains itself.** Every gated button here is disabled for a reason
  the user cannot read off the button, so the disabled tooltip is the only place that
  reason is stated — hence `flux2_gated_button` rather than a hand-threaded
  `add_enabled(..)`.
- **A backend literal this build does not know degrades to "not known"**, with the literal
  on hover, and offers no action. It is never guessed into one of the known states.
- **The two FLUX.2 klein engines draw the SAME body**, differing only where
  `Flux2PanelCtx::variant` says a capability is absent, and both differences are OMISSIONS
  in `install.rs`: no Hugging Face token row for an ungated repository
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
- To change a component row, the preset picker or the forecast line: `components.rs`.
- To change what a line SAYS rather than how it looks: `../decisions.rs`, with its test.
- The tests kept in this directory are CATALOG tests: they assert that every shipped
  locale carries the keys and the `{placeholder}` interpolations this panel spells, which
  a `t!` call cannot check because a unit test runs with no catalog loaded.
