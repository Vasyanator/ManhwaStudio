# 01 — App shell: how an eframe app is wired in egui/eframe 0.36

Ground truth: the vendored crates at
`~/.cargo/registry/src/index.crates.io-*/`{`egui`,`eframe`,`epaint`,`emath`,`ecolor`}`-0.36.2/src/`.
Every API claim below is cited as `egui-0.36.2/src/<file>:<line>` / `eframe-0.36.2/src/<file>:<line>`.
Do not write egui code from memory: 0.36 differs from 0.27–0.31 in the trait shape, the
panel types, and the style API.

## 1. The `eframe::App` trait

The entry point is **`fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)`**, not
`fn update(&mut self, ctx: &Context, frame: &mut Frame)`. There is no `update` method at all.

```rust
// eframe-0.36.2/src/epi.rs:152-236 (trait body, comments trimmed)
pub trait App {
    /// Called once before each call to `Self::ui`. May NOT show any ui or paint.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut Frame) { _ = (ctx, frame); }   // epi.rs:167

    /// Called each time the UI needs repainting. The `Ui` has no margin or background.
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame);                              // epi.rs:182

    /// Only with the "persistence" feature.
    fn save(&mut self, _storage: &mut dyn Storage) {}                                    // epi.rs:212

    #[cfg(feature = "glow")]
    fn on_exit(&mut self, _gl: Option<&glow::Context>) {}                                // epi.rs:222
    #[cfg(not(feature = "glow"))]
    fn on_exit(&mut self) {}                                                             // epi.rs:228

    fn auto_save_interval(&self) -> std::time::Duration { … }                            // epi.rs:234
}
```

* `ui()` is called for the **root viewport only** (`ViewportId::ROOT`); extra native windows come
  from `Context::show_viewport_*` (eframe-0.36.2/src/epi.rs:179-181).
* `logic()` runs before every `ui()` **and also when the window is hidden** but a repaint was
  requested — put polling of background channels there only if it must run while hidden; painting
  is forbidden in it (eframe-0.36.2/src/epi.rs:153-156). While hidden, eframe runs **no egui pass**
  and calls `logic` through `Context::run_logic` (egui-0.36.2/src/context.rs:913): only the window
  state is refreshed, `ctx.input(..)` (events, time) is still the last shown frame's
  (eframe-0.36.2/src/epi.rs:158-162).
* **`on_exit` has a cfg split**: with the `glow` feature it takes `Option<&glow::Context>`, without
  it takes nothing (epi.rs:221-228). This project pins the **glow** renderer
  (`Cargo.toml:45`: `eframe = { version = "0.36.2", default-features = false, features = ["glow", …] }`;
  eframe's default renderer has been wgpu since 0.35, see the comment at `Cargo.toml:41`), so the `Option<&glow::Context>`
  signature is the one that compiles here.

## 2. `&mut Ui` in, `&Context` needed: the clone idiom

`ui()` hands you a `&mut Ui`. Almost every context-level call (`request_repaint`, viewports,
`Window`, `Area`, style, input) needs a `&Context`. `Ui::ctx()` returns `&Context` borrowed **from
the `Ui`** (egui-0.36.2/src/ui.rs:452), so holding it forbids the mutable borrows the panels need.
`Context` is an `Arc` handle, so cloning it is cheap and detaches the borrow. The project's idiom:

```rust
// src/studio.rs:1126
fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    // egui 0.35 replaced `App::update(&Context, …)` with `App::ui(&mut Ui, …)`: the framework
    // now hands us the window-root `Ui` that fills the whole window. The top-level panels below
    // (`Panel::top`, `CentralPanel`) build on this `ui`; floating windows/prompts and every
    // context-level call keep using a borrowed `Context` handle cloned from it. Cloning (Arc
    // inside) is required so the context handle does not keep `ui` borrowed while the root
    // panels mutably borrow it.
    let ctx = ui.ctx().clone();
    let ctx = &ctx;
```

Rule of thumb: clone once at the top of `ui()`, pass `&Context` down for context-level work and
`&mut Ui` down for layout/widgets.

## 3. Panels

egui 0.36 has **one** `Panel` type with side constructors, not `TopBottomPanel`/`SidePanel`:

* `Panel::left(id)` egui-0.36.2/src/containers/panel.rs:249, `Panel::right` :256,
  `Panel::top` :265, `Panel::bottom` :274. Top/bottom are **not resizable by default** (panel.rs:264, :272).
* Builders: `.resizable(bool)` :322, `.show_separator_line(bool)` :362, `.default_size(f32)` :369,
  `.min_size` :380, `.max_size` :387, `.size_range` :394, `.exact_size(f32)` :405, `.frame(Frame)` :413.
  All sizes are **outer** sizes, margins included (panel.rs:214-220).
  There is no `default_width`/`exact_width` on `Panel`; size means width for left/right and height
  for top/bottom.
* `show` takes a **`&mut Ui`**, not a `&Context`:
  `pub fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>`
  (panel.rs:422). `show_inside` is deprecated → renamed to `show` (panel.rs:428).
* Animated variants: `show_collapsible(ui, &mut is_expanded, add_contents)` (panel.rs:451) and
  `show_switched(...)` (panel.rs:563) — the latter animates between a collapsed and an expanded panel;
  give them **distinct ids** (panel.rs:514). Since 0.36 a fully collapsed *resizable* panel keeps an
  invisible grab handle at its fixed edge that drags or double-clicks it back open; opt out with
  `.drag_to_open(false)` (panel.rs:340, :462-464).
* Since 0.36 `show_separator_line(true)` (the default, panel.rs:298) adds the separator's stroke width to the
  frame's **outer margin** on the resizable edge, so the line sits outside the frame outline
  (panel.rs:947-963).
* `CentralPanel::default()` / `::no_frame()` (panel.rs:1193) / `::default_margins()` (panel.rs:1200);
  `show(ui, …)` at panel.rs:1212.
* **Order matters**: first panel added is outermost; `CentralPanel` must be added **last**
  (panel.rs:182-185, :1169). Windows and `Area`s always cover the central panel (panel.rs:1171).

Real skeleton from this repo:

```rust
// src/studio.rs (`StudioApp::ui`: top bar + central content)
egui::Panel::top("top_bar").show(ui, |ui| {
    self.draw_tab_bar(ui);
});
…
egui::CentralPanel::default().show(ui, |ui| {
    canvas.draw(CanvasDrawParams { ctx, ui, /* … */ });
});
```

```rust
// src/tabs/ps_editor/mod.rs:1711, :1774, :1845, :1704 (top + left + right + central)
egui::Panel::top("ps_editor_top").show(ui, |ui| { /* page switcher, zoom */ });

egui::Panel::left("ps_editor_tools")
    .resizable(false)
    .default_size(220.0)
    .show(ui, |ui| { /* tool list + options */ });

let actions = egui::Panel::right("ps_editor_layers")
    .resizable(true)
    .default_size(260.0)
    .show(ui, |ui| self.layers_panel_body(ui))
    .inner;                      // InnerResponse::inner carries the closure's return value

egui::CentralPanel::default().show(ui, |ui| {
    self.draw_canvas(ctx, ui, project);
});
```

## 3.1 Floating panels are NOT egui panels — hard project rule

Everything in §3 is about panels **glued to an edge of the window**: the tab bar, the PS-editor
tool column, the central canvas. Those stay `egui::Panel`.

A **floating** panel — one that hovers over the canvas, has a title strip, collapses, and can be
moved by the user — is not an egui construct here at all. There is exactly one way to make one:

> Declare a **tab** on the panel dock. Never write `Area` + `Frame::popup` with a hand-rolled
> collapse arrow, and never use `egui::Window` as a panel.

A **dialog window** is not a panel: a transient, task-scoped window the user opens for one job and
closes again (the PS editor's layer-effects editor with Apply/Cancel; the page manager's stitch,
split, crop and page-viewer windows) stays an `egui::Window`. `Area` remains for decorations and
temporary surfaces the user can neither dock nor persist (toasts, tooltips, scene-anchored
overlays, the tutorial blocker).

The dock is `crates/ms-widgets/src/panel_dock/` (contract: `dev-docs/dockable_panels_plan.md`, details:
`crates/ms-widgets/src/panel_dock/MODULE_README.md`). Its two widgets are mandatory, not preferred:
`PanelTab` (crates/ms-widgets/src/panel_dock/tab.rs:43) declares one tab, `CollapsiblePanel`
(crates/ms-widgets/src/panel_dock/panel.rs:232) draws one panel. **You never construct either directly** —
the frame driver does. You call `PanelDock::begin` → `.tab(id)` … → `.end(&mut cx)`
(crates/ms-widgets/src/panel_dock/mod.rs:1168, :1186, :1241).

You declare **tabs, not panels.** Which panel a tab lands in, where that panel sits, whether it is
docked to another one or torn off into its own OS window — all of that belongs to the user's saved
layout, not to your call site:

```rust
// src/tabs/typing/tab.rs:1814 — the canonical call site
let mut dock = PanelDock::begin(ctx, dock_state, DockArea {
    rect: area_rect,                  // where panels may live (the canvas area)
    layout_key: AppTab::Typing.key(), // NEVER a localized tab title
});
dock.tab(TYPING_PREVIEW_TAB)
    .title(|| t!("typing.preview.panel_heading")) // localized title, literal TabId
    .visible(tabs_visible.preview)                // a hidden tab is simply not declared this frame
    .min_size(TYPING_PREVIEW_TAB_MIN_SIZE_PX)
    .initial_size(/* … */)                        // FIRST run only; afterwards the layout wins
    .show(|ui, cx: &mut TypingDockCx<'_>| cx.top_panel.draw_preview_tab_body(ui));
// … the other tabs …
drop(dock.end(&mut cx));
```

Consequences worth knowing before you write the body:

* **`TabId` is a literal `&'static str`** and is persisted. It must never come from a localized
  title — a language switch would orphan the user's layout.
* **The body is `FnOnce(&mut Ui, &mut C)`** and captures nothing. `end(cx)` hands your per-frame
  context to one body at a time, which is the only reason several tabs of one frame can touch the
  same heavy state without cloning it or wrapping it in `RefCell`.
* **`initial_size`/`min_size` seed the first frame only.** After that the size is the user's, it is
  persisted, and the solver may shrink it. Do not fight it by forcing a size in the body.
* **The body scrolls on both axes by itself.** Do not add your own `ScrollArea` around the whole
  body; you would nest two scroll areas and the outer one would win.
* **Panels live on `Order::Foreground`.** Canvas input gating is z-order based
  (`ms_input::Condition::PointerOnView`: `Context::layer_id_at` must report `Order::Background` or the
  view's own capture layer), so a panel lowered to `Order::Background` stops
  shielding the canvas underneath it and clicks fall through onto the artwork.

What is still legitimately an `Area`: toasts, tooltips, scene-anchored overlays, the tutorial
blocker — decoration and transient surfaces that the user cannot dock, resize, or persist. See
`06-overlays.md`.

Migration state: the «Текст» tab is fully on the dock (8 tabs). The remaining floating surfaces of
cleaning / translation / ps_editor move over in phases. **A surface not yet migrated is not a
precedent** — new panels have no exemption.

## 4. Startup: `run_native` / `NativeOptions` / `ViewportBuilder` / `CreationContext`

```rust
// eframe-0.36.2/src/lib.rs:288-294
pub fn run_native(
    app_name: &str,
    native_options: NativeOptions,
    app_creator: AppCreator<'_>,
) -> Result
// eframe-0.36.2/src/epi.rs:49-50
pub type AppCreator<'app> =
    Box<dyn 'app + FnOnce(&CreationContext<'_>) -> Result<Box<dyn 'app + App>, DynError>>;
```

`CreationContext` (eframe-0.36.2/src/epi.rs:53) exposes `egui_ctx: egui::Context` — the place to set
fonts/theme **before the first frame**. `NativeOptions` is at epi.rs:296; its `viewport:
ViewportBuilder` field carries window metadata.

**The program has ONE product `run_native`: the main window** (`src/shell/mod.rs::run`). The
launcher and the studio are screens of it, switched in place; nothing closes the window between
them. Abridged:

```rust
// src/shell/mod.rs, `run` (abridged)
let mut viewport = window_geometry::identity::with_identity(
    egui::ViewportBuilder::default()
        .with_title(title.as_str())
        .with_inner_size(MAIN_WINDOW_DEFAULT_INNER_SIZE)
        .with_min_inner_size(min_size),
    config.test_mode,
);
viewport = window_geometry::apply_placement(viewport, &placement);
#[cfg(not(target_os = "windows"))]
{ viewport = viewport.with_maximized(placement.maximized); } // Windows: first-frame command
let native_options = eframe::NativeOptions { viewport, /* android_app on Android */ ..Default::default() };
eframe::run_native(APP_NAME, native_options, Box::new(move |cc| {
    Ok(Box::new(AppShell::new(cc, config, &window_settings, exit_for_window)))
}))
```

The creator (`AppShell::new`) does everything that belongs to the window for its whole life:
monitors, `ui_fonts::install(Tier::Full)`, UI scale, the shader-layer GL backend,
`options_mut(zoom_with_keyboard = false)`, then the first screen (`style::apply`, title, minimum
size). A screen switch later sends `ViewportCommand::Title` (egui-0.36.2/src/viewport.rs:1090) and
`MinInnerSize` (:1111) and reinstalls the complete style (§7).

* **Root close lives in `App::logic`** (eframe-0.36.2/src/epi.rs:167). eframe checks the root
  viewport's output for `ViewportCommand::CancelClose` (viewport.rs:1087) after `logic` + `ui`
  AND after a logic-only tick of a hidden window (eframe-0.36.2/src/native/epi_integration.rs:305-310,
  :340-346), so deciding there also answers the close button of a minimized window. A `Close` the
  app sends itself (viewport.rs:1084) comes back as `close_requested` on the next frame
  (egui-winit-0.36.2/src/lib.rs:1744): never cancel that one, or the window can never close.
* **The other `run_native` sites are separate prompt windows**, run before (or, for the launcher's
  "Update", after) the main window, never at the same time: the missing-environment prompt
  (`ms_installer::prompt_window`, called from `src/startup/prompts.rs`) and the installer / update / existing-install
  windows of `ms-installer` (`install.rs`, `update.rs`). Each installs the core font tier
  (`ui_fonts::install(.., Tier::Core)`) in its own creator.

## 5. Multi-viewport (extra native windows)

**A secondary window of the app (New Project, PSD import, the Wiki guide, batch processing) is an
`ms_widgets::EmbeddedWindow`** (`crates/ms-widgets/src/embedded_window.rs`), never a hand-written
`show_viewport_immediate`: the widget opens the same content either as a separate OS window or
embedded in the window it came from (§5.1). The raw viewport API below is what the widget and the
panel dock's sub-windows are built on. Consumers: `crates/ms-launcher/src/app.rs`
(`draw_new_project_window` / `draw_psd_import_window` / `draw_wiki_guide_window`) and
`crates/ms-launcher/src/new_project/window.rs` (`show_children`: batch processing).

Pattern underneath: a stable `ViewportId`, a `ViewportBuilder`, and
`Context::show_viewport_immediate` (egui-0.36.2/src/context.rs:4116). The child ui callback receives
**`(&mut Ui, ViewportClass)`** — again a `Ui`, not a `Context`.

```rust
// crates/ms-widgets/src/embedded_window.rs, `show_separate` (abridged)
let own_viewport = ViewportId::from_hash_of((SEPARATE_VIEWPORT_SALT, id)); // egui-0.36.2/src/viewport.rs:152
let builder = builder.with_title(title);
ctx.show_viewport_immediate(own_viewport, builder, |ui, class| {
    let child_ctx = ui.ctx().clone();
    response.close_requested = child_ctx.input(|input| input.viewport().close_requested());
    /* CentralPanel + content, unless an embedded window covers this viewport */
});
```

A child viewport's close only REPORTS: `ViewportInfo::close_requested`
(egui-0.36.2/src/data/input/viewport_info.rs:111); the OS window lives for as long as the parent
keeps calling `show_viewport_immediate` for it. `ctx.send_viewport_cmd(egui::ViewportCommand::Close)`
(egui-0.36.2/src/context.rs:4016, `ViewportCommand::Close` at egui-0.36.2/src/viewport.rs:1084)
closes the viewport it is sent from.

Gotchas, all verified in the source docs:

* **Immediate vs deferred.** `show_viewport_immediate` renders the child inline: parent and child
  repaint together, so it is roughly double work per extra viewport; `show_viewport_deferred`
  (context.rs:4062) avoids that but needs `Send + Sync` state. Immediate must be called **every pass**
  the window should exist, and **only from the main thread** (context.rs:4090-4107).
* **Embedding fallback.** If `Context::embed_viewports` is true (wasm, Android's runtime fallback),
  OR the backend registered no immediate-viewport renderer, the callback runs inside a floating
  non-collapsible `egui::Window` and `class == ViewportClass::EmbeddedWindow`
  (context.rs:4124-4127, :4130-4136, `show_embedded_viewport` at :4174-4185;
  egui-0.36.2/src/viewport.rs:82). That wrapper has no close button and never reports
  `close_requested`, and a `CentralPanel` inside it fills the whole root viewport — the
  double-window look `EmbeddedWindow` exists to avoid: in its Embedded mode it never calls the
  viewport API at all.
* **Per-viewport style/visuals.** Each viewport has its own `Ui` tree but shares the `Context` style;
  restyle the child from inside its callback (`ui.set_style`, egui-0.36.2/src/ui.rs:387) rather than
  mutating the global style, or the parent window changes too.
* **Platform quirk in this repo:** Windows misplaces a window created with `with_maximized(true)`, so
  maximisation is deferred to the first child frame via `ViewportCommand::Maximized(true)`
  (egui-0.36.2/src/viewport.rs:1137) + `request_repaint()` — `EmbeddedWindow::maximize_on_open`
  does exactly that on Windows and uses `with_maximized(true)` (viewport.rs:389) elsewhere.

**Do not open a viewport for a floating panel.** A panel torn off into its own OS window is still a
dock tab; the dock owns that window (`crates/ms-widgets/src/panel_dock/window.rs`), draws one
`show_viewport_immediate` per sub-window every frame, persists it, and closes it when its last panel
leaves. Rolling your own viewport for a panel would take it out of the layout, out of persistence,
and out of the user's «Переместить в окно →» menu. See §3.1. In Embedded mode
(`effective_mode`, §5.1) the dock opens no sub-windows: detaching is disabled and stored detached
panels re-dock into the main window (`PanelDockState::apply_window_mode`).

### 5.1 Embedded windows and the stack

`EmbeddedWindow::new(id, title, &mut state)` + `.show(ui, content, children)`; the contract lives in
the file header and `crates/ms-widgets/src/embedded_window/MODULE_README.md`. The egui facts it
rests on:

* **One decision.** `ms_widgets::embedded_window::effective_mode(ctx)` = the user setting
  (`ms_config::secondary_windows::requested_mode()`) unless `ctx.embed_viewports()`
  (egui-0.36.2/src/context.rs:4001) forces Embedded. Evaluated every pass; nothing else decides.
* **Stack storage.** One `WindowStacks` per `Context` in temp data via `Context::data_mut`
  (context.rs:1033) and `IdTypeMap::get_temp_mut_or_default` (egui-0.36.2/src/util/id_type_map.rs:503;
  the value must be `Clone + Send + Sync + Default`), partitioned by `Context::viewport_id`
  (context.rs:3949) and aged by `Context::cumulative_pass_nr` (context.rs:1781), which is PER
  VIEWPORT and restarts when egui drops and recreates a viewport (the `viewports.retain` at
  context.rs:2757).
* **Only the top draws.** Embedded, a window is a `Panel::top` title strip plus a `CentralPanel`
  on the viewport's root `Ui`; the covered windows and the host draw nothing (a host asks
  `covers_host(ctx)` first). Nothing is occluded by z-order — what is not drawn has no widgets,
  so it takes no input.
* **Content state survives** covering and mode switches: the content runs in
  `ui.scope_builder(UiBuilder::new().id(..))` (ui_builder.rs:71) and an explicit id is used
  verbatim (ui.rs:251-252). Per-viewport memory (areas, focus, popups) still resets when a window
  switches between Separate and Embedded.
* **Focus of a widget that stops being drawn.** egui keeps it focused for exactly one more pass and
  then drops it in `Focus::end_pass` (egui-0.36.2/src/memory/mod.rs:630-637: the focused id was
  not used this pass and had focus last pass). `EmbeddedWindow` drops focus itself
  (`Memory::stop_text_input`, memory/mod.rs:1001) whenever the covering window changes, so the new
  top never inherits a hidden widget's focus.
* **Escape and focus.** An unfiltered Escape clears focus in `Focus::begin_pass`, BEFORE the pass
  runs (memory/mod.rs:599-602; `TextEdit`'s default `EventFilter` does not claim Escape,
  egui-0.36.2/src/data/input/event_filter.rs:34, :44, :56). So `memory.focused().is_none()` is always
  true on the pass of an Escape press and cannot tell "Escape left a text field" apart. The widget
  remembers whether focus was held at the end of the window's previous pass and closes only when
  it was not (`close_on_escape(true)`; `InputState::consume_key`,
  egui-0.36.2/src/input_state/mod.rs:722).
* **Separate-mode restore** sends `ViewportCommand::Minimized(false)` and `Focus`
  (viewport.rs:1134, :1169); collapse of a separate window is `Minimized(true)`.

## 6. Repaint model

egui repaints on demand. Nothing in `ui()` re-runs by itself.

* `ctx.request_repaint()` — "if called at least once in a frame, then there will be another frame
  right after this… If called from **outside the UI thread**, the UI thread will wake up and run,
  provided the egui integration has set that up (this works on `eframe`)"
  (egui-0.36.2/src/context.rs:1808-1821).
* `ctx.request_repaint_after(Duration)` (context.rs:1872), `request_repaint_after_secs(f32)`
  (context.rs:1880) — wake after a timeout, i.e. poll.

**You MUST call one of these** whenever state changes outside the frame that produced it: a worker
thread finishing, a channel receiving, an animation you drive yourself. Cheap and idempotent per
frame. Repo examples: while background loaders run (`StudioApp::ui`, src/studio.rs), from the font loader
thread once each stage of the stack is registered (`src/ui_fonts.rs`, `install_blocking`), after a
viewport command (`crates/ms-widgets/src/embedded_window.rs`, `show_separate`, after
`ViewportCommand::Maximized(true)`).

## 7. Theme and style

* **The global style is written ONLY by `ms_theme::install(ctx, style)`** (`crates/ms-theme/src/lib.rs`).
  It is one `ctx.options_mut` call (egui-0.36.2/src/context.rs:1135) that sets the public `Options`
  fields `dark_style`, `light_style` (both the same `Arc<Style>`) and `theme_preference = Dark`
  (egui-0.36.2/src/memory/mod.rs:196-206; `ThemePreference::Dark` at memory/theme.rs:69). The style
  is a pure function of the active screen: `ms_theme::base_style()` (stock `Visuals::dark()` plus
  the `error_fg_color` / `warn_fg_color` status colours) plus that screen's pure layer, e.g.
  `ms_launcher::theme::launcher_style(base)`. Windows without a layer call `ms_theme::apply`
  (= `install(base_style())`). Child viewports share the `Context`, so they inherit it. The main
  window rebuilds and installs the screen's complete style on EVERY screen switch
  (`src/shell/style.rs`); it never compares two `Style`s to skip that (see the `PartialEq` note).
* Do not call `ctx.set_theme` (context.rs:2170), `global_style_mut()` (:2189), `all_styles_mut()`
  (:2213), `style_mut_of` / `set_style_of` (:2237, :2250), `set_global_style` (:2200), or
  `set_visuals` / `set_visuals_of` (:2280, :2267) in product code. **There is no
  `Context::set_style` in 0.36**; `set_style` exists only on `Ui` (egui-0.36.2/src/ui.rs:387) and,
  like `ui.style_mut()`, applies to that subtree only — that stays allowed.
* **Scoped exception:** `ms_theme::with_scoped_global_style(ctx, Arc<Style>, f)` installs a style
  for the duration of `f` and restores the previous `Arc`s and preference exactly (also on
  unwind). It exists for a secondary window that shares the screen's `Context` but must render
  its own look, and is reached only through `EmbeddedWindow::content_style` (the launcher's New
  Project, PSD import and batch-processing windows): the widget runs the content under the scoped
  style AND `ui.set_style`s the content `Ui`, because an immediate viewport's root `Ui` snapshots
  the style at creation.
* `Style: PartialEq` (egui-0.36.2/src/style.rs:241) compares `number_formatter` by `Arc` pointer
  (style.rs:57-61): two independently built styles are never equal.
* `combo_popup_style()` (`crates/ms-launcher/src/theme.rs`) returns a `StyleModifier` for
  popup-local launcher overrides.

## 8. Fonts

egui has two ways to install fonts, and **only one of them is allowed in this repo**:

* `ctx.set_fonts(FontDefinitions)` (egui-0.36.2/src/context.rs:2106) — **forbidden.** It stores
  `memory.new_font_definitions`, and applying it does `self.fonts = None; self.font_definitions =
  font_definitions;` (:548-553), i.e. a FULL REPLACEMENT. Any family another subsystem registered
  at runtime is gone; the typing tab caches its family names without re-checking, so the next use
  panics in epaint (`FontFamily::{family:?} is not bound to any fonts`,
  epaint-0.36.2/src/text/fonts.rs:1024).
* `ctx.add_font(FontInsert)` (egui-0.36.2/src/context.rs:2129) — **the one to use.** Purely
  additive: it pushes into `memory.add_fonts` and the next pass folds each entry into
  `font_definitions` (:556-573). Safe from a worker thread and safe before the first frame.

`FontInsert::new(name, FontData, Vec<InsertFontFamily>)` (epaint-0.36.2/src/text/fonts.rs:481);
font bytes come from `FontData::from_owned(Vec<u8>)` (:133) or `from_static` (:125). Families are
keyed by `FontFamily::{Proportional, Monospace, Name(..)}`.

**Priority is an insertion position, so a loop reverses the chain.** `FontPriority::Highest` is
`fam.insert(0, ..)` and `Lowest` is `fam.push(..)` (egui-0.36.2/src/context.rs:567-568). Iterating
files in ascending name order with `Highest` therefore yields the *reverse* order in the final
fallback chain — walk the sorted list backwards instead.

Never call `ctx.fonts(..)` from a loader: it is `.expect("No fonts available until first call to
Context::run()")` (egui-0.36.2/src/context.rs:1103), and a loader started from a `run_native`
constructor closure runs before the first frame.

```rust
// src/ui_fonts.rs — one add_font per file, from a worker thread
ctx.add_font(egui::epaint::text::FontInsert::new(
    &format!("ms-ui-{stage}-{file_name}"),
    egui::FontData::from_owned(bytes),
    vec![egui::epaint::text::InsertFontFamily {
        family: egui::FontFamily::Proportional,
        priority: egui::epaint::text::FontPriority::Highest,
    }],
));
```

Loading is worker-driven: `ui_fonts::install(&cc.egui_ctx, Tier)` clones the `Context`
(`Arc<RwLock<..>>`, egui-0.36.2/src/context.rs:723 — Send + Sync), spawns a thread, and returns
immediately; no channel and no GUI-thread polling. Every `run_native` constructor closure in the
app must make that call — see the next section for why the work cannot stay on the GUI thread.

## 9. The GUI thread is sacred

`ARCHITECTURE.md` "Key invariants" / global `CLAUDE.md` §5: **the main GUI thread must never block.** Inside `fn ui` (and
`fn logic`) there must be no file I/O, no image decode, no network, no long computation, no blocking
wait on a worker. Kick the work to a thread / `rayon` / async, keep a channel, poll it non-blockingly
in `ui()`, and call `ctx.request_repaint()` (or `request_repaint_after`) so the frame that consumes
the result actually happens. The font loader above is the canonical example.

## Editing map

* To change the root app shell, tab bar, top panel, or per-tab central panel: `src/studio.rs`
  (`impl eframe::App for StudioApp`, src/studio.rs:1126).
* To change process startup or the startup prompts: `src/startup/` (prompt window: `ms_installer::prompt_window`). To change the main window
  (size, icon/app-id, title, close policy, screen switching): `src/shell/` (its `MODULE_README.md`).
* To change the launcher shell: `crates/ms-launcher/src/app.rs`. To change how ANY secondary window
  opens (separate vs embedded, title strip, stacking): `crates/ms-widgets/src/embedded_window.rs`.
* To change the launcher look (Style/Visuals/palette): `crates/ms-launcher/src/theme.rs`.
* To change a studio semantic colour (status text, canvas chrome, checkerboard): `crates/ms-theme/`.
* To change fonts (which files load, family names, fallback order): `src/ui_fonts.rs` and
  `fonts/ui/MODULE_README.md`.
* To change the PS-editor panel layout (top/left/right/central): `src/tabs/ps_editor/mod.rs:1698-1851`.
* To add or change a **floating** panel of a workspace tab: declare a dock tab at that tab's call
  site (`src/tabs/typing/tab.rs:1814` is the reference) — never a new `Area`. To change docking
  behaviour itself (gaps, shrinking, drag, sub-windows): `crates/ms-widgets/src/panel_dock/`, its
  `MODULE_README.md` first.
