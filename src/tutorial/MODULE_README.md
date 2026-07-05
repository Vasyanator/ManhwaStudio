# Module: src/tutorial

## Purpose
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
the egui-0.35 hit-test rationale and the "widgets must be occlusion-aware" caveat
(a widget reading the raw pointer, like the old `WheelSlider`, leaks under the
dim; fix it to use `Response::contains_pointer`/`hovered`).

## Files and submodules
- `engine.rs`: the reusable overlay (`TutorialRegistry`, `TutorialStep<C>`,
  `Tutorial<C>`). Dependency-light (egui + std) because the demo bin
  `src/bin/tutorial_test/main.rs` mounts it via `#[path]`; do not add crate deps
  here. `with_dim_alpha` lets a surface lighten the dim (the launcher does, to
  keep its wallpaper visible).
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
`src/launcher/tutorial.rs`).

## Contracts and invariants
- Per-frame order: `begin_frame()` → optional `maybe_autoplay(id)` (edge-triggered
  by the caller) → `sync(&mut ctx)` before building the UI → `mark(key, rect)` at
  widgets → `render(ctx)` last. `render` persists completion on the finish edge.
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
