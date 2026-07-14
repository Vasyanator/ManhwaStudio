# Widgets: what egui 0.35 gives you, and what this project FORBIDS

Target: `egui`/`eframe` **0.35.0** from crates.io. All egui claims below are cited as
`egui-0.35.0/src/<path>:<line>` against
`~/.cargo/registry/src/index.crates.io-*/egui-0.35.0/src/`. Do not write egui code from
memory of 0.27-0.31 — several APIs on this page did not exist then.

## 0. The hard project rule (read first)

**Do NOT use `egui::Slider` or `egui::ComboBox` directly in product UI** — use the `Wheel*`
replacements from `src/widgets/`. That is the rule as written in `README_AGENT.md:616`. It
extends to `egui::DragValue`, which `WheelSpinBox` wraps for the same reason
(`README_AGENT.md:611`).

Why — two reasons, both invisible in the type system:

1. **Wheel semantics.** Stock egui `Slider`/`DragValue` do not change value on hover+wheel,
   and a wheel event over them scrolls the *parent* `ScrollArea` instead. Every settings
   panel in this app lives inside a `ScrollArea`, so stock widgets would make the panel jump
   under the cursor. The `Wheel*` widgets give one logical step per physical notch and
   consume the event locally (`src/widgets/wheel_slider.rs:12-21`,
   `src/widgets/wheel_combo_box.rs:12-20`).
2. **The process-global wheel guard.** An open combo-box popup floats *over* sliders and
   spin boxes. Without a guard, scrolling that popup's list also drags the slider
   underneath it. The `Wheel*` family shares `wheel_input_guard.rs` to suppress that
   (§2 below). A stock `egui::Slider` does not participate in the guard and will be driven
   by wheel events meant for the popup.

Known exception still in the tree: `src/tabs/typing/tab/panels.rs:399` uses a raw
`egui::Slider`. Treat it as debt, not a precedent.

## 1. The project widget set (`src/widgets/`)

Public surface is `src/widgets/mod.rs:43-72`; the module contract is
`src/widgets/MODULE_README.md`.

| Widget | File | What it is / when to use |
|---|---|---|
| `WheelSlider` | `src/widgets/wheel_slider.rs:39` | `Slider` replacement: hover+wheel = one step (Shift = 5, `SHIFT_WHEEL_STEP_MULTIPLIER`, :37), parent scroll suppressed. Use for every bounded numeric in a panel. |
| `WheelComboBox` | `src/widgets/wheel_combo_box.rs:35` | `ComboBox` replacement for `show_index`-style enums. Wheel cycles the index when closed; when open it publishes the wheel guard. `new` / `from_label` / `from_id_salt`. |
| `WheelSpinBox` | `src/widgets/wheel_spin_box.rs:31` | `DragValue` replacement: unbounded/precise numeric entry with the same wheel contract. |
| `SeedSpinBox` | `src/widgets/seed_spin_box.rs:21` | `u64` seed field + "random" button (`random_seed`, :63+). Self-contained; no `rand` dependency. |
| `AutocompleteLine` | `src/widgets/autocomplete_line.rs` | Single-line input with an inline-completion popup and a configurable suggestion limit. |
| `SpellcheckedTextEdit` | `src/widgets/spellchecked_line.rs` | Multiline `TextEdit` with async Hunspell-compatible spellcheck and misspelling underlines. Dictionary follows the **typesetting** language, never the UI language. |
| `TextEditPlus` | `src/widgets/text_edit_plus.rs` | Multiline editor with per-range text color and ordered rounded background highlights. |
| `EditableComboBox` | `src/widgets/editable_combo_box.rs:38` | Combo box whose value can also be typed freely. Stateful; takes an explicit id source in `new`. |
| `AiButton` | `src/widgets/ai_button.rs:31-96` | Button for an AI tool that gates itself on runtime capabilities (§3). |
| `ViewportColorSelector` | `src/widgets/viewport_color_selector.rs:28` | Color swatch + eyedropper that samples a viewport pixel through egui screenshot events. Stateful, owns a screenshot token. |
| `MarkedScrollArea` | `src/widgets/marked_scroll/` | Vertical scroll area with marks painted on the bar and a gutter of items left of it (§4). |

## 2. `wheel_input_guard.rs` — the process-global popup guard

Contract (`src/widgets/wheel_input_guard.rs:21-86`):

- One `Context::data` temp entry under `Id::new("wheel_input_open_combo_popup_guard")`
  (`:21`) holding `{ frame_nr, rect: Option<Rect> }` (`:24-27`).
- A combo box with an **open popup** calls `publish_combo_popup_open` (`:29`) and, once the
  popup rect is known, `publish_combo_popup_rect` (`:42`).
- Wheel-aware widgets call `combo_popup_open(ctx)` (`:55`) — true when the guard was
  published this frame or the previous one (`:62`) — and skip their wheel reaction.
- They also call `combo_popup_blocks_pointer(ctx)` (`:65`) — true when the pointer is inside
  the published popup rect — and suppress their *hover visuals*, so a slider geometrically
  under an open list neither highlights nor reacts.

Invariant: the guard is valid for **the current frame and the next one only**
(`saturating_sub(guard.frame_nr) <= 1`). It is never persisted. If you write a new wheel-
aware widget, wire both calls; if you write a new popup widget, publish both.

Related 0.35 fact used by these widgets: **`InputState::raw_scroll_delta` no longer
exists**; the unsmoothed per-notch delta is recovered by summing `Event::MouseWheel` events
(`src/widgets/wheel_slider.rs:431-445`).

## 3. `ai_button.rs` — self-gating AI button

Three process-global capability signals live in `src/ai_backend_capabilities.rs:55-58`
(`AtomicU8`, tri-state unknown/yes/no): backend, torch, onnxruntime.

- `AiRequirement` (`src/widgets/ai_button.rs:31`): `Backend | Torch | Onnx | TorchOrOnnx`.
- `AiCaps::current()` (`:96-100`) snapshots the three globals.
- `AiRequirement::is_met(caps, unknown_ok)` (`:52`) is the pure, unit-testable gate;
  `satisfied` (`:43`) is the strict alias (`unknown_ok = false`).

**Rule:** do not hand-thread `ui.add_enabled(torch_available, egui::Button::new(...))` and
re-derive the tooltip text at each call site. Use `AiButton`: it disables itself, explains
why on hover, and stays correct when a capability flips at runtime.

Drawing invariant (`src/widgets/ai_button.rs:16-18`): the optional marker badge is painted
with the **painter only** — it must never allocate a second interactive rect, which would
carve a hole in the button's hitbox.

## 4. `marked_scroll/` — a PORT, not a wrapper

`src/widgets/marked_scroll/bar.rs:9-13`:

> PORT SOURCE: egui 0.33.3, `src/containers/scroll_area.rs`, the per-axis bar block of
> `ScrollArea::show_viewport_dyn` (roughly lines 1200-1443). That code is private, so the
> geometry (`calculate_handle_rect`), handle drag math, floating opacity/width logic, and
> track/handle painting are reproduced here for the vertical axis only.

The host `egui::ScrollArea` is still the scroll *engine* (wheel, drag, momentum, clipping);
its native bars are hidden and the ported bar is painted on top
(`src/widgets/marked_scroll/MODULE_README.md`).

**Warning:** this is a copy of *private* egui internals from a **different version** (0.33.3)
than the one the app links (0.35.0). Do not "modernize" it against 0.35's current
`scroll_area.rs` on sight. It is an explicit upgrade boundary: change it only with a visual
check of handle geometry, floating-bar opacity, and drag anchoring.

## 5. egui 0.35: the widget primitives you actually get

### `Widget`

```rust
// egui-0.35.0/src/widgets/mod.rs:56-66
#[must_use = "You should put this widget in a ui with `ui.add(widget);`"]
pub trait Widget {
    fn ui(self, ui: &mut Ui) -> Response;
}
```

Consumes `self` (widgets are builders, not state). `|ui: &mut Ui| -> Response` also
implements `Widget` (`widgets/mod.rs:55`), and `impl Widget for &mut YourThing` is the
sanctioned escape hatch for stateful widgets (`widgets/mod.rs:53`).

### Adding widgets

| Call | Cite |
|---|---|
| `ui.add(widget) -> Response` | `egui-0.35.0/src/ui.rs:1520` |
| `ui.add_sized(max_size, widget) -> Response` | `egui-0.35.0/src/ui.rs:1537` |
| `ui.add_enabled(enabled, widget) -> Response` | `egui-0.35.0/src/ui.rs:1587` |
| `ui.add_enabled_ui(enabled, add_contents)` | `egui-0.35.0/src/ui.rs:1619` |
| `ui.add_visible(visible, widget)` | `egui-0.35.0/src/ui.rs:1646` |

### Layout containers

- `Ui::scope_builder(UiBuilder, add_contents) -> InnerResponse<R>`
  (`egui-0.35.0/src/ui.rs:2193`). `UiBuilder` carries the id salt
  (`ui_builder.rs:56`) or an explicit id (`ui_builder.rs:72`); this is the modern
  replacement for the old `Ui::child_ui` style.
- `Ui::with_layout(Layout, ...)` (`ui.rs:2469`); `Layout::left_to_right(Align)`
  (`layout.rs:141`), `Layout::top_down(Align)` (`layout.rs:171`).
- `Frame::group(style)` (`containers/frame.rs:178`), `Frame::canvas(style)` (`:227`),
  `Frame::show(ui, add_contents)` (`:404`).
- `Grid::new(id_salt)` (`grid.rs:327`) — note the parameter is an **id salt**, so a
  localized string must not be used as it (see `05-ids-and-i18n.md`).

### `ScrollArea` (has genuinely new API)

- `ScrollArea::vertical()` (`containers/scroll_area.rs:375`), `ScrollArea::new(Vec2b)` (`:394`).
- `ScrollArea::id_salt(...)` (`:482`).
- **`ScrollSource`** (`:192`) — a struct with `scroll_bar: bool`, `mouse_wheel: bool`,
  `drag: DragScroll`; set with `ScrollArea::scroll_source(...)` (`:582`).
- **`DragScroll`** (`:147`) — `Never | OnTouch (default) | Always` (`:150-158`).
  `DragScroll::enabled(ctx)` checks `InputState::has_touch_screen` for `OnTouch` (`:165`).

Older models will reach for `ScrollArea::drag_to_scroll(bool)`. In 0.35 the knob is
`scroll_source(ScrollSource { drag: DragScroll::Always, ..Default::default() })`.

## 6. Atoms — the 0.35 system older models have never seen

egui 0.35 has an **atomics** layer (`egui-0.35.0/src/atomics/`): `atom.rs`, `atoms.rs`,
`atom_kind.rs`, `atom_layout.rs`, `atom_ext.rs`, `sized_atom.rs`, `sized_atom_kind.rs`.

- `Atom<'a>` (`atomics/atom.rs:32`) — "a low-level ui building block ... a piece of text, an
  image, or even a custom widget" (`atom.rs:7-9`), decorated with layout hints
  (`grow`/`shrink`/`align`/`size`).
- `Atoms<'a>` (`atomics/atoms.rs:16`) — an ordered list of them.
- `IntoAtoms<'a>` (`atomics/atoms.rs:209`) — implemented for tuples, so
  `ui.button((image, "Click me!"))` works (`atoms.rs:202-207`).
- `AtomLayout` (`atomics/atom_layout.rs:60`) lays them out and returns
  **`AtomLayoutResponse`** (`atom_layout.rs:701`), which *wraps* a `Response`:

```rust
// egui-0.35.0/src/atomics/atom_layout.rs:701-705
pub struct AtomLayoutResponse {
    pub response: Response,
    custom_rects: SmallVec<[(Id, Rect); 1]>,
}
```

Widgets built on atoms include `Button::new(atoms: impl IntoAtoms<'a>)`
(`widgets/button.rs:45`), `Checkbox::new(checked, atoms: impl IntoAtoms<'a>)`
(`widgets/checkbox.rs:31`), `RadioButton`, `DragValue`, `TextEdit`, and even
`Window::new(title: impl IntoAtoms<'a>)` (`containers/window.rs:101`).

### The practical consequence you WILL hit

`TextEditOutput::response` is **not** a `Response` any more:

```rust
// egui-0.35.0/src/widgets/text_edit/output.rs:6-8
pub struct TextEditOutput {
    pub response: crate::AtomLayoutResponse,
    ...
}
```

So `TextEdit::show(ui).response.rect` does not compile — you need
`.response.response.rect`. The project's wrappers all do exactly that, with the reason
comment inline:

- `src/widgets/text_edit_plus.rs:210-212` — `self.show(ui).response.response` in `Widget::ui`.
- `src/widgets/spellchecked_line.rs:399-401` — same shape.
- `src/widgets/autocomplete_line.rs:86-88` —
  `let text_response = &text_output.response.response;`

If you add a widget that calls `TextEdit::show`, follow the same pattern and keep the
comment: the double `.response.response` looks like a typo otherwise.

## 7. The "double-interface pane" pattern

A settings pane is written **once** as a free function taking `&mut egui::Ui` plus its own
state, and is rendered from **both** surfaces: the studio Settings tab and the launcher
settings page. `src/settings_shared.rs:8` names them "double-interface" panels.

Shared panes today:

| Pane | Renderer |
|---|---|
| General | `draw_general_settings_panel(ui, &mut GeneralSettingsPanelState) -> GeneralSettingsOutcome` — `src/general_settings_panel.rs:181` |
| AI backend | `draw_ai_backend_panel(ui, &AiBackendHandle, &mut AiBackendPanelState)` — `src/ai_backend_panel.rs:182` |
| Tutorials | `draw_tutorials_pane(ui, &TutorialProgressHandle)` — `src/tutorial/settings_pane.rs:31` (behind the `tutorial` feature) |

Glue (`src/settings_shared.rs`): `SettingsSurface { Launcher, Studio }`,
`SettingsSectionId` (the union of sections), the `SECTIONS` registry + `sections_for(surface)`
+ `title_key(id, surface)`, and `SharedSettingsPanels` which **owns the three pane states** so
each surface embeds exactly one instance. Dispatch is `SharedSettingsPanels::draw(id, ui,
surface, &AiBackendHandle) -> SharedSectionOutcome` (`src/settings_shared.rs:286`).

Consumers:
- Studio: `src/tabs/settings/mod.rs:218` (tab bar from `sections_for(Studio)`), `:243`/`:255`
  (`self.shared.draw(...)`).
- Launcher: `src/launcher/pages/settings_page.rs:433` (tab bar from `sections_for(Launcher)`),
  `:363`/`:378` (`self.shared.draw(...)`).

**To add a new shared pane:** (1) write `draw_<name>_pane(ui, &mut State, ...)` in its own
`src/<name>_panel.rs` — no `MangaApp`/launcher types in the signature; (2) add a
`SettingsSectionId` variant and a `SECTIONS` descriptor listing the surfaces and order;
(3) add the state field to `SharedSettingsPanels` and an arm in its `draw`; (4) add the
localized title keys via `title_key`. Do not route a surface-local section through
`SharedSettingsPanels::draw` — it is debug-asserted against (`src/settings_shared.rs:29-32`).

## 8. `egui_extras` is NOT available in app code

`egui_extras` is **not** a dependency of the `manhwastudio_rs` binary. The only reference in
the workspace is `crates/puffin_egui/Cargo.toml:23`
(`egui_extras = { version = "0.35", default-features = false, features = ["serde"] }`), and
`puffin_egui` itself is only pulled in behind the `profiling` feature
(`Cargo.toml:27`).

Therefore **`egui_extras::TableBuilder`, `DatePickerButton`, `Table`, and the `image`
loaders are not in scope in `src/`**. Building a table means hand-rolling `Grid`
(`grid.rs:327`) or a custom layout — or getting sign-off to add the dependency, which is an
architectural change, not a drive-by.

## Editing map

- To add a reusable widget: new file in `src/widgets/`, re-export in `src/widgets/mod.rs`,
  update `src/widgets/MODULE_README.md`.
- To change wheel behaviour or popup suppression: `src/widgets/wheel_input_guard.rs` plus the
  specific `wheel_*.rs` wrapper.
- To gate a new AI tool on runtime availability: `src/widgets/ai_button.rs` +
  `src/ai_backend_capabilities.rs`; never re-derive the gate at the call site.
- To touch the scrollbar port: `src/widgets/marked_scroll/bar.rs` (upgrade boundary — read
  its PORT SOURCE header first).
- To add a settings pane shown in both launcher and studio: `src/settings_shared.rs` +
  a new `src/<name>_panel.rs`; wire both consumers.
- For ids and localization of any of the above: `05-ids-and-i18n.md`.
