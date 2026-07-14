# 06 — Overlays: `Area`, `Order`, layers, and z-order input occlusion

Ground truth: `egui-0.35.0/src/` and `epaint-0.35.0/src/` in the crates.io registry, plus this
repo's overlay code. Every API claim is cited. Read this before painting anything "on top" of the
UI — the difference between *painting* and *registering a hitbox* is the whole subject.

## 1. Layers and `Order`

```rust
// egui-0.35.0/src/layers.rs:10-27
pub enum Order {
    Background,  // Painted behind all floating windows
    Middle,      // Normal moveable windows that you reorder by click
    Foreground,  // Popups, menus etc that should always be painted on top of windows
    Tooltip,     // Things floating on top of everything else, like tooltips
    Debug,       // Debug layer, always painted last / on top
}
```

Bottom to top: `Background < Middle < Foreground < Tooltip < Debug` (the enum derives `Ord`,
layers.rs:8, and `Order::ALL` lists them in that order, layers.rs:31-37). Panels and the central
panel live on `Background`. A layer is identified by `LayerId { order, id }` (layers.rs:65).

Caveat: the doc comment on `Tooltip` says "You cannot interact with these", but
`Order::allow_interaction()` returns `true` for **every** variant (layers.rs:41-47). A widget
allocated in a `Tooltip`-order `Area` *does* take input. What makes `Tooltip` safe for decoration is
that we paint through a `Painter` and never allocate a widget there (§3).

## 2. `Area` — the floating container

`Area::new(Id)` (egui-0.35.0/src/containers/area.rs:133); `Area::show(ctx, …)` takes a **`&Context`**
(area.rs:406), unlike `Panel::show` which takes a `&mut Ui`.

Builders that matter for overlays:

* `.order(Order)` — area.rs:235.
* `.interactable(bool)` — "If false, clicks goes straight through to what is behind us"
  (area.rs:212-218). This is the switch for a click-through decoration area.
* `.enabled(bool)` — content does not respond and is drawn greyed out (area.rs:186-191).
* `.sense(Sense)` — defaults to `Sense::drag()` if movable, `Sense::click()` if interactable, else
  `Sense::hover()` (area.rs:224-228).
* `.fixed_pos`, `.movable(false)`, `.constrain(bool)` (area.rs:277, :198, :287) — `constrain(true)`
  (the default) clamps the area into `Context::content_rect`, which will silently move a callout you
  positioned by hand.
* Full-viewport geometry comes from `ctx.viewport_rect()` (egui-0.35.0/src/context.rs:2819). There is
  no `screen_rect()` in 0.35.

## 3. Painting vs. registering a hitbox — the core distinction

| API | Layer it paints into | Registers a widget / hitbox? |
|---|---|---|
| `Ui::painter()` → `&Painter` (egui-0.35.0/src/ui.rs:457) | the `Ui`'s own layer | **No** — pure paint |
| `Ui::painter_at(rect)` (ui.rs:619) = `painter().with_clip_rect(rect)` (egui-0.35.0/src/painter.rs:71) | same layer, intersected clip | **No** |
| `Context::layer_painter(LayerId)` (egui-0.35.0/src/context.rs:1519) | *any* layer you name, clipped to `content_rect` | **No** |
| `Ui::allocate_rect(rect, sense)` (ui.rs:1256) | — | **Yes**: consumes layout space and creates a `Response` |
| `Ui::interact(rect, id, sense)` (ui.rs:906) / `interact_opt` (ui.rs:911) | — | **Yes**, without consuming layout space |

There is **no `Context::interact`** in 0.35 — interaction is always claimed through a `Ui`.

So: *painting never blocks input*. Blocking input requires a sensed rect in a layer above the
widgets you want to occlude. Conversely, a decoration that must not steal input must be painted with
a `Painter` (or live in an `Area` with `.interactable(false)`).

## 4. Reference implementation: the tutorial overlay

`src/tutorial/engine.rs` is the canonical overlay in this repo; its header (src/tutorial/engine.rs:1-47)
states the contract. Summary of what it does and why:

* **One full-viewport blocker `Area` on `Order::Middle`** (above the panels on `Order::Background`)
  containing exactly **one** `Sense::click_and_drag` rect covering the whole viewport:

```rust
// src/tutorial/engine.rs:706-727
Area::new(Id::new("tutorial_blocker"))
    .order(Order::Middle)
    .fixed_pos(screen.min)
    .constrain(false)
    .movable(false)
    .interactable(true)
    .show(ctx, |ui| {
        ui.set_clip_rect(screen);
        for strip in &strips {
            if strip.is_positive() {
                ui.painter().rect_filled(*strip, 0.0, dim_color);
            }
        }
        // One full-viewport sensor. Because it covers the pointer's search
        // area everywhere, egui's hit-test drops every lower-layer widget
        // from both the click target and the hover set — pure overlap, no
        // per-widget disabling.
        ui.allocate_rect(screen, Sense::click_and_drag());
    });
```

* **Input is blocked by z-order occlusion, not by disabling widgets.** egui's hit-test walks layers
  top-down; once a hit covers the pointer's search area it drops every widget in the layers beneath
  from both the click target and the hover set (engine.rs:17-27).
* **The dim is four strips, the hitbox is one full rect.** The strips (top/bottom/left/right of the
  spotlight hole, built in `paint_dim`, engine.rs:679-704) exist purely for the visual cut-out. Do **not** be tempted to
  make the *hitbox* four strips: they leave the pointer's search area uncovered near the hole and at
  the seams, and hover leaks through (engine.rs:24-27).
* **Consequence:** the spotlighted widget is visually highlighted but also inert — the requirement is
  that nothing under the overlay reacts (engine.rs:28-31).
* **Decorations register no hitbox**: dashed outline + arrow go through `Context::layer_painter` on
  `Order::Tooltip`:

```rust
// src/tutorial/engine.rs:915-932
let painter = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("tutorial_decoration")));
let stroke = Stroke::new(OUTLINE_WIDTH, ACCENT_COLOR);
let corners = [hole.left_top(), hole.right_top(), hole.right_bottom(),
               hole.left_bottom(), hole.left_top()]; // repeat first corner: dashed_line
                                                     // only connects consecutive points
painter.extend(Shape::dashed_line(&corners, stroke, OUTLINE_DASH, OUTLINE_GAP));
painter.arrow(tail, tip - tail, stroke);
```

`Shape::dashed_line(path, stroke, dash_length, gap_length) -> Vec<Shape>`
(epaint-0.35.0/src/shapes/shape.rs:170); `Painter::arrow(origin, vec, stroke)`
(egui-0.35.0/src/painter.rs:417).

* **The interactive callout** is a separate `Area` on `Order::Foreground` (above the blocker) so its
  buttons are clickable while everything below is inert (src/tutorial/engine.rs:751-757).

## 5. THE TRAP: raw pointer reads leak under any overlay

**A widget that decides "am I hovered?" from the raw pointer position cannot be occluded.**
`ctx.input(|i| i.pointer.latest_pos())` / `hover_pos()` return a position with no knowledge of
layers; testing it against your own unclipped rect reacts *through* a modal, a combo popup, or the
tutorial blocker. z-order occlusion cannot save you — the hit-test result is simply not consulted.
This is documented in the repo at src/tutorial/MODULE_README.md:37 and was a real bug in
`WheelSlider`.

Correct pattern — always go through the `Response` produced by `allocate_rect`/`interact`, whose
`hovered()` (egui-0.35.0/src/response.rs:313) and `contains_pointer()` (response.rs:326) are derived
from egui's occlusion-aware frame hit-test:

```rust
// src/widgets/wheel_slider.rs:368-376
fn pointer_over_response_rect(response: &Response) -> bool {
    // Respect element overlap (z-order): rely only on egui's occlusion-aware
    // hit-test. `contains_pointer()` is derived from the frame hit-test, which
    // drops widgets covered by a higher layer, so a combo popup, modal, or
    // tutorial overlay above the slider correctly suppresses its hover/wheel.
    // Never fall back to the raw pointer position (`hover_pos`) against the
    // unclipped rect — that would react through anything drawn on top.
    response.hovered() || response.contains_pointer()
}
```

If you genuinely need a raw position (e.g. hit-testing a warped mesh, as
src/tabs/typing/tab/draw_page.rs:329 does), first gate it on a `Response` that egui hit-tested for
you, or on the layer under the pointer:

```rust
// src/input_util.rs:55-61 — "is the pointer over floating UI rather than bare canvas?"
pub fn pointer_over_floating_area(ctx: &egui::Context) -> bool {
    let Some(pos) = ctx.input(|i| i.pointer.interact_pos()) else { return false; };
    ctx.layer_id_at(pos)                       // egui-0.35.0/src/context.rs:3002
        .is_some_and(|layer| layer.order != egui::Order::Background)
}
```

Note the warning in that file's doc comment (src/input_util.rs:44-53): 0.35's
`Context::is_pointer_over_egui` (egui-0.35.0/src/context.rs:2841) is **not** a substitute — a
space-filling `CentralPanel` leaves the root ui's available rect empty, so it reports `true`
everywhere over the central content.

## 6. Claiming input for a manually painted region

`Sense::hover()` / `click()` / `drag()` / `click_and_drag()` (egui-0.35.0/src/sense.rs:45, :60, :68, :81).

* Inside a layout: `let r = ui.allocate_rect(rect, Sense::click_and_drag());` (ui.rs:1256) — reserves
  the space **and** senses it.
* Over already-laid-out content (an overlay handle on top of a canvas):
  `let r = ui.interact(rect, id, Sense::drag());` (ui.rs:906) — senses without consuming layout space;
  use a stable, unique `Id`.
* Then branch on `r.clicked()` (response.rs:183), `r.hovered()`, `r.contains_pointer()`,
  `r.drag_delta()` — never on raw pointer maths (§5).

Order of allocation within a layer matters: a later-allocated rect wins over an earlier one covering
the same point.

## 7. Popups, menus, tooltips

* `Popup` (egui-0.35.0/src/containers/popup.rs:165): `Popup::new(id, ctx, anchor, layer_id)` (:190),
  `Popup::from_response(&Response)` (:215), `from_toggle_button_response` (:228), `menu(&Response)`
  (:235), `context_menu(&Response)` (:246). Anchors accept a `Rect`, a `Pos2` or a `&Response`
  (popup.rs:38-50).
* `PopupKind::order()` decides the layer: `Tooltip => Order::Tooltip`, `Menu | Popup =>
  Order::Foreground` (popup.rs:145-151). So popups and menus sit **above** an `Order::Middle` overlay
  and remain interactive over it — which is exactly what the tutorial callout relies on, and exactly
  what you must remember when you want an overlay to block *everything*.
* Global helpers: `Popup::is_any_open(ctx)` (popup.rs:660), `Popup::close_all(ctx)` (popup.rs:677).
* `Response::context_menu(add_contents)` (egui-0.35.0/src/response.rs:1008) and
  `context_menu_opened()` (:1015) are the convenience wrappers.
* `Tooltip` (egui-0.35.0/src/containers/tooltip.rs:8): `Tooltip::for_widget(&Response)` (:39),
  `for_enabled` (:53), `for_disabled` (:62), `.at_pointer()` (:72), `.show(|ui| …)` (:101);
  `Response::on_hover_text` (response.rs:707) is the shorthand.

## 8. Canvas overlays (pointer only)

Tabs paint on top of the page image through the `CanvasHooks` trait
(`src/canvas/mod.rs:415` — `draw_canvas_overlay_on_page`, `draw_canvas_mask_overlay_on_page`,
`draw_canvas_overlay_top_left`, `canvas_scrollbar_marks`, …), called from `src/canvas/scene.rs`
(e.g. src/canvas/scene.rs:954), which paints via `ui.painter().with_clip_rect(clip_rect)`
(src/canvas/scene.rs:995) — i.e. paint-only, no hitbox, same rule as §3. The canvas has its own
`src/canvas/MODULE_README.md`; read it before touching that layer.

## Editing map

* To change the tutorial overlay (dim, spotlight, blocker, callout, arrow): `src/tutorial/engine.rs`
  (contract in its header, engine.rs:1-47; module notes in `src/tutorial/MODULE_README.md`).
* To change "did the click land on bare canvas vs. floating UI": `src/input_util.rs`.
* To change hover/wheel occlusion behavior of a custom widget: `src/widgets/wheel_slider.rs` is the
  worked example of the correct `Response`-based pattern.
* To change what tabs paint over the page: the `CanvasHooks` impls in `src/tabs/*/`, dispatched from
  `src/canvas/scene.rs`; see `src/canvas/MODULE_README.md`.
