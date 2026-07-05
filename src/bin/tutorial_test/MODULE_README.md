# Module: src/bin/tutorial_test

## Purpose
Standalone debug binary for developing and visually verifying the tutorial /
onboarding overlay engine: a dim-everything-except-one-element highlight with a
dashed outline, a text callout biased toward the screen centre, and an arrow from
the callout to the highlight. It exists so the overlay can be iterated on against
real application widgets without running the full editor.

## Architecture
`main.rs` is the `eframe::App` (uses `fn ui`, like the main program). It builds a
tabbed window from the same reusable widgets the app uses — `WheelSlider`,
`WheelSpinBox`, `WheelComboBox` — mounted with `#[path = "../../widgets/..."]`
because the package has no library target. The wheel widgets reference
`super::wheel_input_guard`, so they are mounted as children of one `mod widgets`
parent that also mounts `wheel_input_guard`. Do not fork these files; they must
stay usable by the main application.

Callout/arrow placement (`compute_placement` + `classify_zone` in `tutorial.rs`):
the viewport is split into 8 zones by rays from its centre to the points that
divide each side into equal thirds — the middle third of a side is a straight
zone, the outer thirds of two adjacent sides form a corner zone. The highlight's
centre picks the zone. The arrow arrives at the highlight's centre-facing
edge/corner (opposite the zone): straight (axis-aligned) for a side zone, 45° for
a corner zone. It has a FIXED length (`ARROW_LEN`) back toward the centre, and the
callout is anchored by its element-facing edge/corner to the arrow tail, so the
arrow leaves the callout at the mirror of where it enters the highlight. The
callout width is fixed and its height is taken from the previous frame's measured
size (stable per step) so the fixed-length arrow stays aligned.

The overlay engine now lives in the main binary at `src/tutorial/engine.rs`; this
bin mounts it via `#[path = "../../tutorial/engine.rs"] mod tutorial;` so the demo
and the production overlay can never diverge. The engine is the reusable part:
- `TutorialRegistry` — per-frame map of `&'static str` key -> `Rect`. The UI
  calls `begin_frame()` once, then `mark(key, response.rect)` at each addressable
  widget. This is the whole coupling between UI and tutorial: pointing a step at a
  real element is one `mark` line, no UI restructuring.
- `TutorialStep` — target key(s) + title + body.
- `Tutorial<C>` — ordered steps, current index, `render(ctx, &registry)`.

Driving app state (open a tab, set a mode) without touching the UI: a step may
carry an `on_enter` closure that mutates the app context `C` (the engine is
generic over it; the demo uses `C = Tab`). The app calls `tutorial.sync(&mut ctx)`
at the START of the frame, before building the UI; `sync` runs the current step's
`on_enter` once per entry (re-armed on every step change, so Назад/Далее re-run
it). Because it runs before the UI is built, a step that opens a tab draws that
tab's highlight target the same frame. The tab-switching widget code is untouched
— the side effect lives in the tutorial script (`build_steps`).

Overlay mechanics (verified against egui 0.35):
- Input blocking is pure hitbox overlap, not per-widget disabling: ONE
  full-viewport `Area` on `Order::Middle` (above panels on `Order::Background`)
  allocates a single `Sense::click_and_drag` rect over the whole screen. egui's
  hit-test drops every lower-layer widget from both the click target and the
  hover set once a top hit covers the pointer's search area; a full-screen sensor
  covers it everywhere, so nothing beneath reacts.
- Why NOT four strips around the hole: egui's hover uses
  `WidgetHits::contains_pointer`, which keeps lower-layer widgets unless a single
  top hit covers the whole search area. Four strips leave that area uncovered
  near the hole and at the seams, so hover (and stray slider drags) leak to the
  widgets underneath. The full-screen sensor is the fix.
- Consequence: the highlighted element is spotlighted (its region is not dimmed)
  but inert, like everything else — the contract is "nothing under the overlay
  reacts". The dim is drawn as four strips around the hole for the VISUAL cut-out
  only; the hitbox is separate and full-screen. (If a future step must let the
  user click the highlighted element, that needs a different design — the layer
  system cannot expose a live hole while still absorbing hover around it.)
- Decoration (dashed outline + arrow) is drawn via `Context::layer_painter` on
  `Order::Tooltip`, which registers no widget and adds no hitbox.
- Widgets must respect z-order for the hitbox to cover them. `WheelSlider`
  previously detected hover from the raw pointer position (`hover_pos` vs its
  rect), bypassing occlusion, so it reacted through any overlay. It was fixed in
  `src/widgets/wheel_slider.rs` (`pointer_over_response_rect`) to rely only on
  egui's occlusion-aware hit-test (`Response::contains_pointer`), so the overlay
  layer now suppresses it like every other widget — no per-widget disabling.

## Files and submodules
- `main.rs`: eframe app, tabbed demo UI, target-rect recording, demo tutorial
  script (`build_steps`). Mounts the engine from `src/tutorial/engine.rs`.
- (engine lives at `src/tutorial/engine.rs`; see `src/tutorial/MODULE_README.md`.)

## Contracts and invariants
- egui 0.35 specifics: `Context::viewport_rect()` (not the removed
  `screen_rect()`); `Painter::rect_stroke` takes a `StrokeKind` (this module uses
  `rect_filled` + `dashed_line` instead); close a dashed rect by repeating the
  first corner.
- The registry is frame-scoped: `begin_frame()` before building the UI; targets
  not built this frame are simply absent (a step with no visible target dims the
  whole screen and centres the callout).
- Test-binary rules (`../MODULE_README.md`): no fake behaviour in shared modules;
  startup errors go to stderr.

## Editing map
- To change highlight/dim/arrow visuals or placement, edit
  `src/tutorial/engine.rs` (shared with the app).
- To add demo widgets or steps, edit `main.rs` (`build_steps` + the tab bodies).
- The engine is already integrated into the app (`src/tutorial/`, launcher tour +
  Settings "Обучение" pane); production step scripts live next to their UI.

## Verify
`EGUI_INSPECTION=1 cargo run --bin tutorial_test --features inspection`, then
attach the `egui` MCP server to `127.0.0.1:5719`.
