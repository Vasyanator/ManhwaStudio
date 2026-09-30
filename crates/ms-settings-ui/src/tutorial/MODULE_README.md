# Module: crates/ms-settings-ui/src/tutorial

## Purpose
Gated behind the `tutorial` cargo feature (off by default; the root package's
`tutorial` feature forwards to `ms-settings-ui/tutorial` and `ms-launcher/tutorial`,
the only root feature forwarded to workspace crates): the integration
points (`mod tutorial`, controller fields, `mark`/autoplay/`sync`/`render` calls,
and the "Обучение" settings panes) are `#[cfg(feature = "tutorial")]` in their
host surfaces, so a default build compiles the whole subsystem out. This module's
own files still compile — the demo bin `src/bin/tutorial_test` mounts `engine.rs`
via `#[path]` independently of the feature.

Shared in-app tutorial / onboarding layer. One overlay engine dims the viewport
except a spotlighted target, draws a dashed outline + arrow + text callout, and
absorbs all input beneath it. Each *tutorial* is a named list of steps; a surface
(launcher, studio) owns a controller that runs one tutorial at a time and
persists per-tutorial completion.

## Architecture
```
TutorialId ──key──► config "Tutorials" section
    ▲                     ▲
    │                     │ load / persist
TutorialProgress (completed-set + autoplay), shared via TutorialProgressHandle
    ▲                                   ▲
    │ shared handle                     │ shared handle
TutorialController<C>  ──starts──►  Tutorial<C> (engine.rs overlay)
    │                                   │
 registry.mark(key,rect) at widgets   render(ctx, &registry) draws overlay
```
`C` is the surface context a step's `on_enter` mutates (e.g. `LauncherState`) to
navigate the UI to the next highlight. The controller enforces one active
tutorial per surface, autoplays unseen tutorials on entry (caller edge-triggers
`maybe_autoplay`), and records completion on the finish/skip edge in `render`.

Input blocking is pure hitbox overlap (one full-viewport `Sense::click_and_drag`
`Area` on `Order::Middle`), NOT per-widget disabling — see `engine.rs` header for
the egui hit-test rationale (written against 0.35; the 0.36 hit-test only adds skipping of
non-interactable areas, `egui-0.36.2/src/context.rs:476-482`) and the "widgets must be occlusion-aware" caveat
(a widget reading the raw pointer, like the old `WheelSlider`, leaks under the
dim; fix it to use `Response::contains_pointer`/`hovered`).

## Files and submodules
- `engine.rs`: the reusable overlay (`TutorialRegistry`, `TutorialStep<C>`,
  `Tutorial<C>`). Dependency-light (egui + std, so it uses `eprintln!` not the
  structured logger) because the demo bin `src/bin/tutorial_test/main.rs` mounts
  it via `#[path]`; do not add crate deps here. Step builders: `.id()` (label for
  jumps), `.on_enter()`, `.choice(label, goto)` (branch button), `.await_gate()`
  (auto-advance when the gate holds — a spinner shows meanwhile), `.gated()`
  (disable "Далее" until the gate holds), `.link()/.finish()` (override where the
  step goes). `TutorialStep::message()` builds a target-less step (whole viewport
  dimmed, centred callout). Navigation is a history stack, so "Назад" is correct
  across branch jumps. `with_dim_alpha`/`with_callout_tint` let a surface soften
  the dim / back the callout (the launcher does, to keep its wallpaper visible).
- `id.rs`: `TutorialId` central enum — stable persistence `key`, display `title`,
  `is_available` (which ids the replay pane shows), exhaustive `ALL`.
- `progress.rs`: `TutorialProgress` (completed-set + autoplay) + persistence to
  the config `Tutorials` section; `TutorialProgressHandle` shared handle;
  `shared_progress()`. Writes are offloaded to a background thread.
- `controller.rs`: `TutorialController<C>` — registry + active tutorial + catalog
  (`TutorialId -> steps fn`) + progress handle. Autoplay + completion edge.
- `settings_pane.rs`: `draw_tutorials_pane` — surface-agnostic replay pane reused
  by the studio Settings tab and the launcher settings page (double interface,
  like `crate::ai_backend_panel`). Depends only on the progress handle.

Per-surface step scripts live next to their UI, NOT here (e.g.
`crates/ms-launcher/src/tutorial.rs`).

## Gates and driving actions
A gate (`.await_gate`/`.gated`) is a `Fn(&GateCtx<C>) -> bool` evaluated in `sync`
(has `&C` + the previous frame's registry). To wait on an app state, `C` must
expose it; to wait on an element, use `g.has_target(key)`. `sync` runs `on_enter`
on the ENTRY frame WITHOUT evaluating the gate (so an action triggered in
`on_enter` takes effect before the gate is first checked), and advances at most
one step per frame.

When a step must trigger something the surface guards behind `&mut self` (the
new-project pipeline), the step can't hold a reference to it. Use a
COMMAND/SNAPSHOT context: `C` carries a state snapshot (read by gates) plus a
command queue (written by `on_enter`); the surface drains and executes the
commands after `sync` returns. See `crates/ms-launcher/src/new_project/tutorial.rs` +
`window.rs`.

## Contracts and invariants
- Per-frame order: optional `maybe_autoplay(id)` (edge-triggered by the caller) →
  `sync(&mut ctx)` before building the UI → `begin_frame()` → `mark(key, rect)` at
  widgets → `render(ctx)` last. `sync` uses the previous frame's registry (still
  intact until `begin_frame`); `render` persists completion on the finish edge.
- A branch id in `.choice`/`.link(Goto(..))` must match some step's `.id`; a
  dangling id ends the tutorial (logged) rather than hanging.
- `TutorialId::key` is the on-disk key: never change an existing value.
- The overlay covers only its own viewport; detached child viewports are not
  dimmed.
- Progress persistence never runs on the GUI thread (background write).

## Editing map
- Add a tutorial: add a `TutorialId` variant (+ `key`/`title`/`is_available`),
  write a `steps()` script next to its UI, register it in that surface's
  controller catalog, and `mark` its target rects in the UI.
- Change overlay visuals / placement / dim: `engine.rs`.
- Change the replay pane: `settings_pane.rs`.

## Verify
`bash .claude/skills/egui-mcp/launch.sh "" -- --test-launcher` then attach the
`egui` MCP server; the launcher tour autoplays on first entry.
