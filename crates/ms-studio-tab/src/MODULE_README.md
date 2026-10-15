# Module: crates/ms-studio-tab/src

## Purpose
Declares `StudioTab`, the order-free per-tab hook surface of the studio. The studio root
(`src/studio/tabs.rs`, `Tabs`) maps an `AppTab` to `&dyn StudioTab` once and iterates the
hooks, instead of matching on `AppTab` at every call site that asks "what does this tab need".

## Architecture
A single-file trait crate. Every tab crate implements the trait in its own `studio_tab.rs`
(the translation host, cleaning, typing, PS editor, page manager, the four simple tabs) and the
root implements it for the Settings tab. Every method has a no-op default, so an
implementation states only the hooks the tab really has.

Dependencies: `ms-widgets` (`DockLayout`), `ms-input` (`Declarations`, `ContextId`), `ms-memory`
(eviction request / report), `ms-project` (`ProjectData`), `egui` (`Context`), `serde_json`
(exit-settings root).
Layer: its own line between `ms-models / ms-tabs-simple / …` and
`ms-project / ms-widgets / ms-native-runtime` (see `ARCHITECTURE.md` §2).

## Files and submodules
- `lib.rs`: the trait and the object-safety / default-behaviour test.

## Contracts and invariants
- **Order-free hooks only.** The root may call a hook for every tab in `AppTab::ALL` order; the
  outcome must not depend on that order. Drawing and every ordered sequence (staging writers,
  save-to-project, page ops, discard, shutdown barriers) stay explicit root code, never a hook.
- An implementation never touches the staging writers, never reads another tab, never blocks
  and does no I/O — except `flush_debounced_stores_blocking`, the teardown-only hook.
- `write_exit_settings` is pure: the root owns the single `user_config` read-modify-write that
  runs every tab's writer (keys written with `ms_config::set_section_bool`).
- `default_dock_layout` returning `Some` is the one answer to "does this tab host the panel dock".
- Input: `input_declarations` (called once, when the studio builds its input registry) returns
  the tab's actions; their ids are persisted user overrides and must never change.
  `input_contexts` (called every frame for the ACTIVE tab, before the input router runs) lists
  the contexts the tab publishes. There is no execute hook: a tab reads what fired from the
  `ms_input::InputFrame` its draw receives. A canvas tab also forwards its canvas's contexts
  (`CanvasView::input_contexts`: `canvas.editable`, `canvas.zoom_accepted`).
- The trait stays object safe (no generic methods, no `Self` by value); a test guards it.
- This crate must never depend on a tab crate, `ms-models` or the root package.

## Editing map
- A new order-free per-tab question: add a defaulted method here, implement it in the tabs
  that need it, iterate it in `Tabs`. An ORDERED step does not belong here.
- To change what one tab does for a hook: that tab crate's `studio_tab.rs`, not this crate.
