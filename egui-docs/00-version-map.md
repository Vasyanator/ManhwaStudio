# egui 0.36 version map — "what you remember" vs. what exists

**Version stamp:** egui / eframe / epaint / emath / ecolor **0.36.2** (crates.io), upstream commit
`49682f8baa058bf49e011035cfbd6e825f88a5ef` (from `egui-0.36.2/.cargo_vcs_info.json`). Declared in
`Cargo.toml:45-46`.

**How to use this page.** If you are about to write egui code from memory (you were trained on 0.27–0.31),
read the tables below first. Every row is verified against the vendored sources under
`~/.cargo/registry/src/index.crates.io-*/`; citations are `crate-dir/relative/path.rs:LINE`.
"0 hits" claims were produced by grepping the whole crate `src/` tree. If an API you remember is not on
this page and you cannot find it with `grep -rn` in the vendored source, **it does not exist** — do not
write it.

Grep root for verification:
`~/.cargo/registry/src/index.crates.io-*/egui-0.36.2/src`

---

## 1. The five things that break every stale-memory patch

| What older egui had | egui 0.36 | Citation |
|---|---|---|
| `impl eframe::App { fn update(&mut self, ctx: &Context, frame: &mut Frame) }` | `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)` — the **only** required method. `fn update` is **0 hits** in `eframe-0.36.2/src/epi.rs`. Get the context via `ui.ctx()` (clone it if you need an owned handle). | `eframe-0.36.2/src/epi.rs:182` |
| (no equivalent) | `fn logic(&mut self, ctx: &egui::Context, frame: &mut Frame)` — optional, called once before each `ui()` and also when the UI is hidden but a repaint was requested. **No painting allowed inside.** While hidden, eframe runs **no egui pass**, only `Context::run_logic`, so `ctx.input(..)` still shows the last shown frame. | `eframe-0.36.2/src/epi.rs:167`, `:158-162`; `egui-0.36.2/src/context.rs:913` |
| `SidePanel::left("id")`, `TopBottomPanel::top("id")` | One unified `Panel` struct: `Panel::left(id)` / `right` / `top` / `bottom`. `SidePanel`: **0 hits**. `TopBottomPanel`: **0 hits**. | `egui-0.36.2/src/containers/panel.rs:206,249,256,265,274` |
| `panel.show(ctx, ...)` | `Panel::show(self, ui: &mut Ui, add_contents) -> InnerResponse<R>` — takes **`&mut Ui`**, not `&Context`. Same for `CentralPanel::show`. | `containers/panel.rs:422`, `containers/panel.rs:1212` |
| `ctx.screen_rect()` | **0 hits** (`pub fn screen_rect` does not exist on `Context`; `screen_rect` survives only as a field of `RawInput`, `data/input/raw_input.rs:38`). Use `ctx.content_rect()` or `ctx.viewport_rect()`. | `egui-0.36.2/src/context.rs:2907,2921` |
| `input.raw_scroll_delta` | **0 hits**. Only `smooth_scroll_delta` remains. See §5. | `egui-0.36.2/src/input_state/mod.rs:241` |

---

## 2. Panels (read this before touching any layout)

```rust
// egui 0.36, inside `fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame)`
egui::Panel::top(egui::Id::new("toolbar")).show(ui, |ui| { /* … */ });
egui::Panel::left(egui::Id::new("side")).resizable(true).default_size(240.0).show(ui, |ui| { /* … */ });
egui::CentralPanel::default().show(ui, |ui| { /* … */ });
```

| Item | Signature / note | Citation |
|---|---|---|
| `Panel::left/right/top/bottom` | `fn left(id: impl Into<Id>) -> Self`. The id argument is an **`Id`**, not a `&str` "id source"; `&str` works only where it `Into<Id>`. `top`/`bottom` default to `resizable(false)`. | `containers/panel.rs:249,265` |
| Builder | `resizable`, `show_separator_line`, `default_size`, `min_size`, `max_size`, `size_range(impl Into<Rangef>)`, `exact_size`, `frame`, `drag_to_open` (0.36; default `true`: a collapsed resizable panel keeps an invisible grab handle that drags or double-clicks it open). Sizes are **outer** sizes (include `Frame` margin). With `show_separator_line(true)` 0.36 widens the frame's **outer margin** on the resizable edge by the separator stroke width, so the line sits outside the frame (`panel.rs:947-963`). | `containers/panel.rs:322,362,369,380,387,394,405,413,340` |
| `Panel::show` | `fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` | `containers/panel.rs:422` |
| `Panel::show_inside` | `#[deprecated = "Renamed to `show`"]` | `containers/panel.rs:427` |
| Animated panels | `show_collapsible(ui, is_expanded, add_contents)`; `show_switched(...)`; `show_animated_inside` / `show_animated_between_inside` are deprecated renames. | `containers/panel.rs:451,563,496,659` |
| `CentralPanel` | `#[derive(Default)]`; `CentralPanel::default()`, `::no_frame()`, `::default_margins()`, `.frame(Frame)`, `.show(ui, ..)`. `show_inside` deprecated. | `containers/panel.rs:1187,1193,1200,1212,1217` |

---

## 3. Context rects: `content_rect` vs `viewport_rect`

Both round to UI pixels and both exist on `Context`:

* `ctx.content_rect()` — the area **safe for content**: `viewport_rect` minus safe-area insets (OS status bar,
  notch, dynamic island). Use this for layout. `context.rs:2907`.
* `ctx.viewport_rect()` — the **full** area available to egui, *including* the region under the status
  bar/notch. Use this only if you want to paint behind those. `context.rs:2921`.
* The insets themselves: `InputState::content_rect()` subtracts `safe_area_insets` from `viewport_rect`
  (`input_state/mod.rs:510,257,259`).

On desktop the two are usually identical; do not assume it.

---

## 4. Painter, corners, strokes

| What older egui had | egui 0.36 | Citation |
|---|---|---|
| `Rounding` | `CornerRadius` (re-exported at `egui::CornerRadius`). `Rounding`: **0 hits** in `egui-0.36.2/src`. | `epaint-0.36.2/src/corner_radius.rs:13`, `egui-0.36.2/src/lib.rs:447` |
| `painter.rect_filled(rect, rounding, color)` | `rect_filled(&self, rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>) -> ShapeIdx` (integers work: `CornerRadius::same(6)` or just `6`). | `egui-0.36.2/src/painter.rs:397` |
| `painter.rect_stroke(rect, rounding, stroke)` | `rect_stroke(&self, rect, corner_radius, stroke, stroke_kind: StrokeKind) -> ShapeIdx` — **4th arg is mandatory**. | `egui-0.36.2/src/painter.rs:406` |
| `painter.rect(rect, rounding, fill, stroke)` | `rect(&self, rect, corner_radius, fill_color, stroke, stroke_kind: StrokeKind)` — 5 args. | `egui-0.36.2/src/painter.rs:380` |
| — | `StrokeKind::{Inside, Middle, Outside}`; re-exported as `egui::StrokeKind`. `Middle` reproduces the old centred-stroke look. | `epaint-0.36.2/src/stroke.rs:102-111`, `egui-0.36.2/src/lib.rs:447` |
| `Visuals::window_rounding` | `Visuals::window_corner_radius` / `menu_corner_radius` (`window_rounding`: **0 hits**). | `egui-0.36.2/src/style.rs:1061,1069` |
| `Frame::none()` | `Frame::NONE` (const) or `Frame::new()`. `Frame::none()`: **0 hits**. Other constructors: `Frame::group(style)`, `::central_panel(style)`, `::window(style)`, `::menu(style)`, `::popup(style)`, `::canvas(style)`. Builder method is `.corner_radius(..)`, not `.rounding(..)`. | `containers/frame.rs:161,173,178,277` |

---

## 5. Scrolling / wheel input (trap)

`InputState::raw_scroll_delta` is gone (**0 hits**). What exists:

* `InputState::smooth_scroll_delta: Vec2` — smoothed, **and it is zeroed when the zoom modifier
  (Ctrl/Cmd) is held**: under the zoom modifier egui redirects the wheel into `zoom_factor_delta`
  instead. (`input_state/mod.rs:241`, and the `is_zoom` branch at `input_state/mod.rs:459-467`;
  the zero-init is at `input_state/mod.rs:453`.)
* `Event::MouseWheel { unit: MouseWheelUnit, delta: Vec2, phase, modifiers }` — the raw per-event delta,
  unit-dependent (`Point`/`Line`/`Page`). (`egui-0.36.2/src/data/input/event.rs:150`.)
* `InputState` also keeps a `wheel: WheelState` accumulator, but the field is **private**
  (`input_state/mod.rs:227` — no `pub`), so app code cannot reach it. Its
  `unprocessed_wheel_delta` / `smooth_wheel_delta` (`input_state/wheel_state.rs:53,67`) are an
  implementation detail of smoothing, not a replacement for the old raw delta. Do not plan around them.

**In this repo:** `src/input_util.rs:31` `raw_wheel_delta(&egui::InputState) -> Vec2` reconstructs the old
semantics by summing this frame's `Event::MouseWheel` deltas. Use it for Ctrl+wheel zoom/rotate; do not
read `smooth_scroll_delta` for modifier-gated wheel handling.

---

## 6. Pointer-over-egui (trap)

* `Context::is_pointer_over_area`: **0 hits** — removed.
* `Context::is_pointer_over_egui()` exists (`context.rs:2943`) but is **not** a drop-in replacement. Its
  implementation: if the layer under the pointer is `Order::Background`, it returns
  `!root_ui_available_rect.contains(pos)` (`context.rs:2951-2958`). An app whose root `Ui` is fully
  consumed by a space-filling `CentralPanel` leaves `root_ui_available_rect` empty, so this returns
  `true` for *every* point over the central content.
* Workaround used in this repo (`src/input_util.rs:55`, `pointer_over_floating_area`): take
  `ctx.layer_id_at(pos)` (`context.rs:3104`) and test `layer.order != egui::Order::Background`
  (`egui-0.36.2/src/layers.rs:10-27`). That answers "is the pointer over a floating Window/menu/popup/
  tooltip rather than the canvas", which is what canvas hit-testing actually needs.

---

## 7. Ids

| What older egui had | egui 0.36 | Citation |
|---|---|---|
| `.id_source(x)` on ComboBox / CollapsingHeader / ScrollArea / Resize / UiBuilder | `.id_salt(x)` (`impl AsIdSalt`). | `containers/collapsing_header.rs:434`, `containers/scroll_area.rs:482`, `containers/resize.rs:81`, `ui_builder.rs:56` |
| `ComboBox::from_id_source(x)` | `ComboBox::new(id_salt, label)` or `ComboBox::from_id_salt(x)` / `from_label(text)`. No `id_source` on `ComboBox`. | `containers/combo_box.rs:54,69,85` |
| — | `TextEdit::id_source` still exists but only forwards to `id_salt`; prefer `id_salt`. | `widgets/text_edit/builder.rs:175,181` |

---

## 8. TextEdit output (trap)

`TextEdit::show(ui) -> TextEditOutput` (`widgets/text_edit/builder.rs:452`), and

```rust
pub struct TextEditOutput {
    pub response: crate::AtomLayoutResponse,   // NOT egui::Response
    pub galley: Arc<Galley>,
    pub galley_pos: Pos2,
    pub text_clip_rect: Rect,
    pub state: TextEditState,
    pub cursor_range: Option<CCursorRange>,
}
```
(`widgets/text_edit/output.rs:6-24`.)

`AtomLayoutResponse` has a public field `response: Response` (`atomics/atom_layout.rs:701-702`) and
`impl Deref/DerefMut<Target = Response>` (`atomics/atom_layout.rs:729`). So `out.response.changed()`
still compiles via deref, but where a real `Response` **value** is needed (returning it, `|`-ing two
responses, storing it) write `out.response.response`.

---

## 9. Atoms (unknown to older models)

`egui::atomics::*` is re-exported at the crate root (`lib.rs:463`). Buttons, menu
buttons and window titles no longer take `impl Into<WidgetText>`; they take **`impl IntoAtoms<'a>`**:

* `Button::new(atoms: impl IntoAtoms<'a>)` (`widgets/button.rs:45`), plus `Button::image` /
  `Button::image_and_text` (`widgets/button.rs:89,97`).
* `Window::new(title: impl IntoAtoms<'a>)` (`containers/window.rs:102`).
* `Ui::menu_button(atoms: impl IntoAtoms<'a>, add_contents)` (`ui.rs:2788`).
* Core types: `Atom<'a>` (`atomics/atom.rs:32`), `Atoms<'a>` (`atomics/atoms.rs:16`), `AtomKind<'a>`
  (`atomics/atom_kind.rs:26`), `AtomLayout<'a>` (`atomics/atom_layout.rs:60`),
  `trait IntoAtoms<'a>` (`atomics/atoms.rs:209`), `trait AtomExt<'a>` (`atomics/atom_ext.rs:7`).
* `AtomExt` decorates anything atom-like: `.atom_id(Id)`, `.atom_size(Vec2)`, `.atom_grow(bool)`,
  `.atom_shrink(bool)`, `.atom_max_size(Vec2)`, `.atom_max_width(f32)`, `.atom_max_height(f32)`,
  `.atom_align(Align2)` (`atomics/atom_ext.rs:12-76`).
* Tuples of atoms are atoms: `Button::new(("★", "Save"))` style composition. `Atom::custom` reserves a
  rect you retrieve afterwards with `AtomLayoutResponse::rect(id)` (`atomics/atom_layout.rs:719-726`).

Practical rule: a bare `&str`, `RichText` or `Image` still works as before because they implement
`IntoAtoms`; the difference bites when you pass something exotic or destructure the output.

---

## 10. Menus, popups, modals, sides (new/renamed containers)

| What older egui had | egui 0.36 | Citation |
|---|---|---|
| `egui::menu::bar(ui, \|ui\| …)` | `MenuBar::new().ui(ui, \|ui\| …)` (`pub fn bar(`: **0 hits**). Re-exported as `egui::MenuBar` (`lib.rs:464`). | `containers/menu.rs:217,232,257` |
| `ui.menu_button(text, …)` | Still exists, not deprecated; dispatches to `MenuButton` or `SubMenuButton` depending on `menu::is_in_menu(ui)`. | `ui.rs:2788-2798` |
| `response.context_menu(…)` | Still exists, not deprecated; implemented as `Popup::context_menu(self).show(add_contents)`, returns `Option<InnerResponse<()>>`. | `response.rs:1028-1029` |
| `ui.close_menu()` | **0 hits**. Use `Ui::close()` (`ui.rs:1040`), plus `close_kind` / `should_close` / `will_parent_close`. | `ui.rs:1040` |
| `egui::popup::popup_below_widget` etc. | `Popup<'a>` builder: `Popup::new(id, ctx, anchor, layer_id)`, `::from_response`, `::from_toggle_button_response`, `::menu`, `::context_menu`; then `.show(\|ui\| …) -> Option<InnerResponse<R>>`. Config: `.open_bool`, `.close_behavior(PopupCloseBehavior)`, `.at_pointer()`, `.align(RectAlign)`, `.gap`, `.frame`, `.width`, `.interactable(bool)` (0.36). | `containers/popup.rs:165,191,217,230,237,248,508,378` |
| — | `Modal::new(id).show(ctx, \|ui\| …) -> ModalResponse<T>`, with `should_close()`. Note: `show` takes **`&Context`**, unlike panels. | `containers/modal.rs:16,26,77,151` |
| — | `Sides::new().show(ui, \|ui\| left, \|ui\| right)` — left/right-justified row. | `containers/sides.rs:45,62,145` |

---

## 11. Ui construction, styles, theme

| What older egui had | egui 0.36 | Citation |
|---|---|---|
| `ui.allocate_new_ui(builder, …)`, `ui.allocate_ui_at_rect(rect, …)`, `ui.child_ui(…)` | **0 hits** for `allocate_new_ui` and `allocate_ui_at_rect`. Use `ui.scope_builder(UiBuilder::new().max_rect(r).layout(l), \|ui\| …)` (`ui.rs:2194`) or `Ui::new(ctx, id, UiBuilder)` for a root ui (`ui.rs:109`). | `egui-0.36.2/src/ui.rs:2194,109` |
| — | `UiBuilder`: `new()`, `id_salt`, `id`, `layer_id`, `max_rect`, `layout`, `disabled()`, `invisible()`, `sizing_pass()`, `style`, `sense`, `closable()`, `accessibility_parent`. | `ui_builder.rs:46-193` |
| `ctx.set_style(..)` / `ctx.style_mut(..)` | On `Context` these are **gone** (only `Ui::style_mut` at `ui.rs:380` and `Ui::set_style` at `ui.rs:387` exist). Use `ctx.all_styles_mut(..)`, `ctx.style_mut_of(Theme, ..)`, `ctx.set_style_of(Theme, ..)`, `ctx.set_visuals(..)` / `set_visuals_of(Theme, ..)`. | `context.rs:2213,2237,2250,2267,2280` |
| — | `ctx.theme() -> Theme`, `ctx.set_theme(impl Into<ThemePreference>)`. | `context.rs:2158,2170` |
| `Sense::hover()/click()/drag()/click_and_drag()` | Same constructors, but `Sense` is now a **bitflags `struct Sense(u8)`** with `Sense::HOVER/CLICK/DRAG/FOCUSABLE`; you can `|` them. | `sense.rs:4,6-22,45,60,68,81` |
| `Widget` trait | Unchanged shape: `fn ui(self, ui: &mut Ui) -> Response`. 0.36 adds a provided `boxed()` returning `BoxedWidget<'a>` for dynamic dispatch. | `widgets/mod.rs:63-71,75,13` |
| `Order` | Unchanged: `Background, Middle, Foreground, Tooltip, Debug`. `Area::new(id).order(..).fixed_pos(..).show(ctx, ..)`. | `layers.rs:10-27`, `containers/area.rs:133,235,277,406` |

---

## 12. Textures and images

| Item | 0.36 | Citation |
|---|---|---|
| `ctx.load_texture(name, image, options) -> TextureHandle` | Unchanged. | `context.rs:2390` |
| `ColorImage` fields | Still `pub size: [usize; 2]`, `pub pixels: Vec<Color32>` — **plus a new `pub source_size: Vec2`**, so struct-literal construction breaks. | `epaint-0.36.2/src/image.rs:48-57` |
| `ColorImage::new(size, Color32)` | **Changed meaning**: `ColorImage::new(size: [usize;2], pixels: Vec<Color32>)`. To fill with a color use `ColorImage::filled(size, color)`. | `epaint-0.36.2/src/image.rs:61,75` |
| Converters | `from_rgba_unmultiplied`, `from_rgba_premultiplied`, `from_gray`, `from_gray_iter`, `from_rgb`, `example`. | `epaint-0.36.2/src/image.rs:113,128,146,163,193,209` |
| `Image` widget | `Image::new(impl Into<ImageSource>)`, `Image::from_texture(impl Into<SizedTexture>)`, `.max_size`, `.fit_to_exact_size`, `.paint_at(ui, rect)`. `egui::include_image!` still exists. | `widgets/image.rs:64,102,147,177,369`, `lib.rs:535` |
| Loaders | `egui_extras::install_image_loaders(ctx)`. | `egui_extras-0.36.2/src/loaders.rs:58` |
| Tables/strips | `egui_extras::TableBuilder`, `StripBuilder` still exist. | `egui_extras-0.36.2/src/table.rs:247`, `strip.rs:44` |

> **`egui_extras` is not a dependency of this binary.** It is pulled in only by
> `crates/puffin_egui`, behind the `profiling` feature, so nothing in this table is reachable
> from `src/` as things stand. Adding the dependency is a deliberate decision, not a detail.
> See `04-widgets.md` §8.

---

## 13. eframe entry points

| Item | 0.36 | Citation |
|---|---|---|
| `eframe::run_native(app_name, native_options, app_creator)` | Exists. `AppCreator<'app> = Box<dyn FnOnce(&CreationContext<'_>) -> Result<Box<dyn App>, DynError>>` — note the creator returns a **`Result`**. | `eframe-0.36.2/src/lib.rs:288`, `epi.rs:49-50` |
| `eframe::run_simple_native` | **0 hits** — removed. | (grep) |
| `eframe::run_native_ext(name, opts, Option<egui::Context>, creator)` | New: lets you pass a pre-built `egui::Context`. | `eframe-0.36.2/src/lib.rs:306` |
| `App::save`, `App::on_exit`, `App::clear_color`, `App::raw_input_hook` | Still present. | `epi.rs:212,222,248,279` |
| `eframe::Frame` | Still the app-frame handle passed to `ui`/`logic`. | `epi.rs:661` |

Canonical 0.36 app shape (as used here, `src/app.rs:2337-2338`):

```rust
impl eframe::App for MangaApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();           // clone: `ui` stays mutably usable
        egui::Panel::top(egui::Id::new("top")).show(ui, |ui| { /* … */ });
        egui::CentralPanel::default().show(ui, |ui| { /* … */ });
        egui::Window::new("floating").show(&ctx, |ui| { /* … */ }); // Window still takes &Context
    }
}
```

`Window::show(self, ctx: &Context, …) -> Option<InnerResponse<Option<R>>>` (`containers/window.rs:543`)
and `Modal::show(self, ctx: &Context, …)` (`containers/modal.rs:77`) still take a `&Context` — only
**panels** switched to `&mut Ui`.

---

## 14. Changed between 0.35 and 0.36

| Item | 0.36 | Citation |
|---|---|---|
| `RawInput::modifiers` | **Field removed.** Read `InputState::modifiers` (`ctx.input(\|i\| i.modifiers)`); backends now send `Event::ModifiersChanged(Modifiers)`. | `egui-0.36.2/src/input_state/mod.rs:318`, `data/input/event.rs:73` |
| `RawInput::dropped_files` | Now `Vec<DroppedFileHandle>` (`Arc<dyn DroppedFile>`), `serde`-skipped. `DroppedFile` is a **trait**: `path()`, `bytes()` (native) / `bytes_async()` (wasm); egui never reads the contents itself. | `data/input/raw_input.rs:81`, `data/input/dropped_file.rs:10,15,33,45` |
| `Visuals::clip_rect_margin` | `#[deprecated]`, **no effect**. Content margin of a scroll area is `ScrollArea::content_margin(..)` / `style.spacing.scroll.content_margin` (default `Margin::ZERO`). | `style.rs:1086-1087`, `containers/scroll_area.rs:631`, `style.rs:509` |
| `App::logic` while hidden | Called through `Context::run_logic` (no egui pass; returns `LogicOutput`). | `eframe-0.36.2/src/epi.rs:158-162`, `egui-0.36.2/src/context.rs:913` |
| `Panel` | `drag_to_open` (default on) and separator room in the outer margin — see §2. | `containers/panel.rs:340,947-963` |
| `Popup` | Opens with a **sizing pass** on its first frame (`.sizing_pass(!was_open_last_frame)`); `.interactable(bool)` added. | `containers/popup.rs:587,378` |
| `TextEdit::event_filter(EventFilter)` | New builder: which keys (arrows / tab / escape) the focused edit captures. | `widgets/text_edit/builder.rs:357` |
| `Window::title_frame(Frame)` | New builder for the title-bar frame. | `containers/window.rs:272` |
| `Spacing::extra_text_line_spacing` | New `pub` field (default `0.0`); struct literals of `Spacing` must name it. | `style.rs:424` |
| `DebugOptions::warn_if_rect_changes_id` | Default is now `false` in debug builds too. | `style.rs:1404` |
| `TexturesDelta` drop | `Drop` **`debug_assert!`s** the delta is empty. A headless test that discards a `FullOutput` (`ctx.run_ui(..)`, `ctx.end_pass()`) aborts; call `FullOutput::drop_without_applying_deltas()` instead. | `epaint-0.36.2/src/textures.rs:335-344`, `egui-0.36.2/src/data/output.rs:72-76` |
| `Options::sync_window_theme` | Defaults to `true`: `set_theme(Dark)` also darkens the native title bar. | `memory/mod.rs:229,333` |
| NaN widget rect | `debug_assert!(!rect.any_nan())` on every `ui.interact` / `allocate_rect`. Painting a NaN rect is still allowed. | `context.rs:1254` |
| Tooltips | A tooltip with no interactive widget is **not interactable** — hover passes through to the widget below. See `06-overlays.md`. | `containers/tooltip.rs:132-140` |

---

## NAME COLLISIONS IN THIS REPO

* **`TextShape`** — the typing renderer's own bubble-shape enum
  `ms_text_render::types::TextShape { Free, Rectangle, Oval, Hexagon, SoftPeak }`
  (`crates/ms-text-render/src/types.rs:467`; a separate copy exists in the test binary
  `src/bin/text_render_test/render.rs:72`). This is **not** `epaint::TextShape`, which is egui's
  text-drawing shape struct (`epaint-0.36.2/src/shapes/text_shape.rs:12`, reachable as
  `egui::epaint::TextShape`). Never `use egui::epaint::TextShape` in typing code.
* **`TextWrapMode`** — `ms_text_render::types::TextWrapMode { None, WholeWords, Minimal, Moderate, … }`
  (`crates/ms-text-render/src/types.rs:476`) collides with `egui::TextWrapMode`
  (re-exported from epaint at `egui-0.36.2/src/lib.rs:475`). Two different types; check the import.
* **`Shape` / `Mesh`** — the repo does **not** define its own `Shape` or `Mesh`. `Mesh` in
  `src/tabs/typing/tab/mesh_geometry.rs`, `src/tabs/typing/tab/doc_layers.rs` and
  `src/tabs/ps_editor/layer_render.rs` **is** `egui::Mesh` (re-export of `epaint::Mesh`,
  `egui-0.36.2/src/lib.rs:447`), and `Shape` in `src/tabs/ps_editor/layer_render.rs:23` is `egui::Shape`.
  The repo-specific shape concept is spelled `TextShape` (above).
