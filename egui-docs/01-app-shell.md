# 01 — App shell: how an eframe app is wired in egui/eframe 0.35

Ground truth: the vendored crates at
`~/.cargo/registry/src/index.crates.io-*/`{`egui`,`eframe`,`epaint`,`emath`,`ecolor`}`-0.35.0/src/`.
Every API claim below is cited as `egui-0.35.0/src/<file>:<line>` / `eframe-0.35.0/src/<file>:<line>`.
Do not write egui code from memory: 0.35 differs from 0.27–0.31 in the trait shape, the
panel types, and the style API.

## 1. The `eframe::App` trait

The entry point is **`fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)`**, not
`fn update(&mut self, ctx: &Context, frame: &mut Frame)`. There is no `update` method at all.

```rust
// eframe-0.35.0/src/epi.rs:152-230 (trait body, comments trimmed)
pub trait App {
    /// Called once before each call to `Self::ui`. May NOT show any ui or paint.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut Frame) { _ = (ctx, frame); }   // epi.rs:161

    /// Called each time the UI needs repainting. The `Ui` has no margin or background.
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame);                              // epi.rs:176

    /// Only with the "persistence" feature.
    fn save(&mut self, _storage: &mut dyn Storage) {}                                    // epi.rs:206

    #[cfg(feature = "glow")]
    fn on_exit(&mut self, _gl: Option<&glow::Context>) {}                                // epi.rs:216
    #[cfg(not(feature = "glow"))]
    fn on_exit(&mut self) {}                                                             // epi.rs:222

    fn auto_save_interval(&self) -> std::time::Duration { … }                            // epi.rs:228
}
```

* `ui()` is called for the **root viewport only** (`ViewportId::ROOT`); extra native windows come
  from `Context::show_viewport_*` (eframe-0.35.0/src/epi.rs:173-175).
* `logic()` runs before every `ui()` **and also when the window is hidden** but a repaint was
  requested — put polling of background channels there only if it must run while hidden; painting
  is forbidden in it (eframe-0.35.0/src/epi.rs:153-156).
* **`on_exit` has a cfg split**: with the `glow` feature it takes `Option<&glow::Context>`, without
  it takes nothing (epi.rs:215-222). This project pins the **glow** renderer
  (`Cargo.toml:62`: `eframe = { version = "0.35", default-features = false, features = ["glow", …] }`;
  0.35's eframe default is wgpu, see the comment at `Cargo.toml:59`), so the `Option<&glow::Context>`
  signature is the one that compiles here.

## 2. `&mut Ui` in, `&Context` needed: the clone idiom

`ui()` hands you a `&mut Ui`. Almost every context-level call (`request_repaint`, viewports,
`Window`, `Area`, style, input) needs a `&Context`. `Ui::ctx()` returns `&Context` borrowed **from
the `Ui`** (egui-0.35.0/src/ui.rs:451), so holding it forbids the mutable borrows the panels need.
`Context` is an `Arc` handle, so cloning it is cheap and detaches the borrow. The project's idiom:

```rust
// src/app.rs:2338-2346
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

0.35 has **one** `Panel` type with side constructors, not `TopBottomPanel`/`SidePanel`:

* `Panel::left(id)` egui-0.35.0/src/containers/panel.rs:222, `Panel::right` :229,
  `Panel::top` :238, `Panel::bottom` :247. Top/bottom are **not resizable by default** (panel.rs:237, :245).
* Builders: `.resizable(bool)` :294, `.show_separator_line(bool)` :303, `.default_size(f32)` :310,
  `.min_size` :321, `.max_size` :328, `.size_range` :335, `.exact_size(f32)` :346, `.frame(Frame)` :354.
  All sizes are **outer** sizes, margins included (panel.rs:187-193).
  There is no `default_width`/`exact_width` on `Panel`; size means width for left/right and height
  for top/bottom.
* `show` takes a **`&mut Ui`**, not a `&Context`:
  `pub fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>`
  (panel.rs:363). `show_inside` is deprecated → renamed to `show` (panel.rs:369).
* Animated variants: `show_collapsible(ui, &mut is_expanded, add_contents)` (panel.rs:389) and
  `show_switched(...)` (panel.rs:500) — the latter animates between a collapsed and an expanded panel;
  give them **distinct ids** (panel.rs:451).
* `CentralPanel::default()` / `::no_frame()` (panel.rs:1045) / `::default_margins()` (panel.rs:1052);
  `show(ui, …)` at panel.rs:1064.
* **Order matters**: first panel added is outermost; `CentralPanel` must be added **last**
  (panel.rs:156-159, :1021). Windows and `Area`s always cover the central panel (panel.rs:1023).

Real skeleton from this repo:

```rust
// src/app.rs:2449 and src/app.rs:2487 (root app: top bar + central content)
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

## 4. Startup: `run_native` / `NativeOptions` / `ViewportBuilder` / `CreationContext`

```rust
// eframe-0.35.0/src/lib.rs:288-294
pub fn run_native(
    app_name: &str,
    native_options: NativeOptions,
    app_creator: AppCreator<'_>,
) -> Result
// eframe-0.35.0/src/epi.rs:49-50
pub type AppCreator<'app> =
    Box<dyn 'app + FnOnce(&CreationContext<'_>) -> Result<Box<dyn 'app + App>, DynError>>;
```

`CreationContext` (eframe-0.35.0/src/epi.rs:53) exposes `egui_ctx: egui::Context` — the place to set
fonts/theme **before the first frame**. `NativeOptions` is at epi.rs:290; its `viewport:
ViewportBuilder` field carries window metadata. The project's studio startup:

```rust
// src/main.rs:1363-1393
let mut viewport = egui::ViewportBuilder::default()
    .with_inner_size([1400.0, 900.0])
    .with_min_inner_size([900.0, 600.0])
    .with_app_id(MAIN_WINDOW_APP_ID);
#[cfg(not(target_os = "windows"))]
{ viewport = viewport.with_maximized(true); }
if let Some(icon) = load_embedded_icon_data() { viewport = viewport.with_icon(icon); }

let native_options = eframe::NativeOptions { viewport, ..Default::default() };

eframe::run_native(
    &title,
    native_options,
    Box::new(move |cc| {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        Ok(Box::new(app::MangaApp::new(project, ai_backend.clone(), flag_for_app)))
    }),
)
```

Other `run_native` entry points in the same file: missing-python prompt (src/main.rs:672),
update check (src/main.rs:1178), launcher (src/main.rs:1676).

## 5. Multi-viewport (extra native windows)

Pattern used for all secondary windows: a stable `ViewportId`, a `ViewportBuilder`, and
`Context::show_viewport_immediate` (egui-0.35.0/src/context.rs:4014). The child ui callback receives
**`(&mut Ui, ViewportClass)`** — again a `Ui`, not a `Context`.

```rust
// src/launcher/app.rs:620-646 (new-project window)
let viewport_id = egui::ViewportId::from_hash_of(NEW_PROJECT_VIEWPORT_ID_SALT); // egui-0.35.0/src/viewport.rs:153
let builder = crate::launcher::apply_launcher_window_metadata(
    egui::ViewportBuilder::default()
        .with_title(t!("launcher.new_project.window_title"))
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([1000.0, 680.0])
        .with_app_id(&self.app_id)
        .with_resizable(true) /* … */,
);
ctx.show_viewport_immediate(viewport_id, builder, |ui, class| {
    keep_open = self.new_project_window.show(ui, class);
});
```

Close it with `ctx.send_viewport_cmd(egui::ViewportCommand::Close)`
(egui-0.35.0/src/context.rs:3914, `ViewportCommand::Close` at egui-0.35.0/src/viewport.rs:1085) —
see src/launcher/app.rs:662, :721.

Gotchas, all verified in the source docs:

* **Immediate vs deferred.** `show_viewport_immediate` renders the child inline: parent and child
  repaint together, so it is roughly double work per extra viewport; `show_viewport_deferred`
  (context.rs:3960) avoids that but needs `Send + Sync` state. Immediate must be called **every pass**
  the window should exist, and **only from the main thread** (context.rs:3988-4005).
* **Embedding fallback.** If `Context::embed_viewports` is true (backend without multi-window
  support, e.g. wasm), the callback is run inside an embedded `Window` and `class ==
  ViewportClass::EmbeddedWindow` (context.rs:4008-4011, egui-0.35.0/src/viewport.rs:83). Child code
  must therefore not assume it owns an OS window — that is why the repo passes `class` down.
* **Per-viewport style/visuals.** Each viewport has its own `Ui` tree but shares the `Context` style;
  restyle the child from inside its callback (`ui.set_style`, egui-0.35.0/src/ui.rs:386) rather than
  mutating the global style, or the parent window changes too.
* **Platform quirk in this repo:** Windows misplaces a window created with `with_maximized(true)`, so
  maximisation is deferred to the first child frame via
  `ViewportCommand::Maximized(true)` + `request_repaint()` (src/launcher/app.rs:633-644).

Other users of the pattern: src/launcher/psd_import_window.rs via src/launcher/app.rs:699,
src/launcher/new_project/window.rs:2047 (screen capture) and :6757
(src/launcher/new_project/batch_processing/window.rs).

## 6. Repaint model

egui repaints on demand. Nothing in `ui()` re-runs by itself.

* `ctx.request_repaint()` — "if called at least once in a frame, then there will be another frame
  right after this… If called from **outside the UI thread**, the UI thread will wake up and run,
  provided the egui integration has set that up (this works on `eframe`)"
  (egui-0.35.0/src/context.rs:1740-1753).
* `ctx.request_repaint_after(Duration)` (context.rs:1804), `request_repaint_after_secs(f32)`
  (context.rs:1812) — wake after a timeout, i.e. poll.

**You MUST call one of these** whenever state changes outside the frame that produced it: a worker
thread finishing, a channel receiving, an animation you drive yourself. Cheap and idempotent per
frame. Repo examples: while background loaders run (src/app.rs:2443-2447), polling a font-load
channel with a 100 ms delay (src/app.rs:2259), after a viewport command
(src/launcher/app.rs:643).

## 7. Theme and style

* `ctx.set_theme(egui::Theme::Dark)` — takes `impl Into<ThemePreference>` (egui-0.35.0/src/context.rs:2102);
  used at src/main.rs:1387.
* **There is no `Context::set_style` in 0.35.** The context-level API is
  `global_style()` / `global_style_mut()` (context.rs:2107, :2121), `all_styles_mut()` (:2145),
  `style_mut_of(theme, …)` / `set_style_of(theme, …)` (:2169, :2182), `set_visuals` / `set_visuals_of`
  (:2212, :2199). `set_style` exists only on `Ui` (egui-0.35.0/src/ui.rs:386) and applies to that
  subtree.
* The launcher's palette lives in `src/launcher/theme.rs`: `configure_context(ctx)` clones
  `ctx.global_style()`, edits `Style`/`Visuals` (spacing, `Visuals::dark()`, widget fills, corner
  radii) and installs it (src/launcher/theme.rs:45-…); `combo_popup_style()` returns a
  `StyleModifier` (src/launcher/theme.rs:160) for popup-local overrides.

## 8. Fonts

`ctx.set_fonts(FontDefinitions)` (egui-0.35.0/src/context.rs:2038). Font bytes are `Arc<FontData>`;
`FontData::from_owned(Vec<u8>)` (epaint-0.35.0/src/text/fonts.rs:139) and `from_static`
(:131). Families are keyed by `FontFamily::{Proportional, Monospace, Name(..)}`.

```rust
// src/app.rs:2296-2332 (system-font injection), applied at src/app.rs:2250 via ctx.set_fonts(...)
defs.font_data.insert(
    regular_font_name.clone(),
    Arc::new(egui::FontData::from_owned(result.regular_bytes)),
);
defs.families
    .entry(egui::FontFamily::Proportional)
    .or_default()
    .insert(0, regular_font_name.clone());          // index 0 = highest priority
let bold_family = defs
    .families
    .entry(egui::FontFamily::Name("system-ui-sans-bold".into()))
    .or_default();
```

Font loading itself happens on a worker thread and is applied when the channel yields
(src/app.rs:2240-2262) — see the next section for why.

## 9. The GUI thread is sacred

`README_AGENT.md` / `CLAUDE.md` §5: **the main GUI thread must never block.** Inside `fn ui` (and
`fn logic`) there must be no file I/O, no image decode, no network, no long computation, no blocking
wait on a worker. Kick the work to a thread / `rayon` / async, keep a channel, poll it non-blockingly
in `ui()`, and call `ctx.request_repaint()` (or `request_repaint_after`) so the frame that consumes
the result actually happens. The font loader above is the canonical example.

## Editing map

* To change the root app shell, tab bar, top panel, or per-tab central panel: `src/app.rs`
  (`impl eframe::App for MangaApp`, src/app.rs:2337).
* To change process startup, window size/icon/app-id, or which `run_native` runs: `src/main.rs`.
* To change the launcher shell and its secondary native windows: `src/launcher/app.rs`.
* To change the launcher look (Style/Visuals/palette): `src/launcher/theme.rs`.
* To change fonts: `build_system_font_definitions` in `src/app.rs:2296`.
* To change the PS-editor panel layout (top/left/right/central): `src/tabs/ps_editor/mod.rs:1698-1851`.
