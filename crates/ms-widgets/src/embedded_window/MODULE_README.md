# Module: crates/ms-widgets/src/embedded_window

## Purpose
Submodules of `embedded_window.rs` (the `EmbeddedWindow` widget): the pure stack model of
embedded secondary windows and the widget's headless tests.

## Architecture
`embedded_window.rs` (the parent file) owns everything that touches egui: the mode decision
(`effective_mode`), the host pre-check (`covers_host`), the caller-owned
`EmbeddedWindowState`, the per-pass `EmbeddedWindow` builder and its two drawing paths
(Embedded: title strip + `CentralPanel` on the viewport's root `Ui`; Separate:
`show_viewport_immediate`). It keeps ONE `WindowStacks` per `egui::Context` in temp data
(`IdTypeMap::get_temp_mut_or_default`, egui-0.36.2/src/util/id_type_map.rs:503) and only reads or
mutates it through the methods of `stack.rs`.

Per pass, per viewport: the first `covers_host` / `show` of the pass calls `decide`, which prunes
entries not shown in this or the previous pass and fixes the covering window (topmost fresh,
non-collapsed) for the rest of the pass. A `show` decides BEFORE it touches its own entry, so a
window entering the stack never covers in its first pass. When the decision differs from the
previous pass, the parent file drops keyboard focus (`Memory::stop_text_input`), because egui
keeps a no-longer-drawn widget focused for one more pass (egui-0.36.2/src/memory/mod.rs:630-637).

## Files and submodules
- `stack.rs`: `WindowStacks` (per-viewport stacks, parent scope of a running `children` closure,
  per-pass `Decision`), `StackEntry`, and the pure `escape_closes` rule. No `Context`, no drawing;
  the only egui types are `Id` and `ViewportId`. Unit-tested in place.
- `tests.rs`: headless `Context::default()` tests of the widget (its `embed_viewports` is true,
  so every window there is embedded): stacking, collapse/restore, parent close, content id, Escape.

## Contracts and invariants
- At most one window draws per viewport per pass; the decision of a pass never changes inside it.
- `touch` keeps an existing entry's position; only `raise` (restore / re-open) reorders, and it
  moves the window together with its subtree. `remove` also removes the subtree (windows whose
  `parent` chain reaches it), so a closed parent never leaves a child covering a viewport.
- Fresh = shown in this pass or the previous one. A `last_pass` greater than the current pass is
  stale: egui recreates a removed viewport with a new pass counter (egui-0.36.2/src/context.rs:2757).
- Escape closes only the covering window, only when enabled, and only when no widget held focus
  at the end of the window's previous pass AND none holds it now: egui drops focus on an
  unfiltered Escape before the pass runs (egui-0.36.2/src/memory/mod.rs:599-602), so the current
  focus alone cannot tell "Escape left a text field" from "Escape on an idle window".

## Editing map
- To change which window covers, pruning, or how close/raise treat children: `stack.rs` (add the
  case to its tests first).
- To change drawing, chrome, modes or focus handling: the parent `embedded_window.rs`.
