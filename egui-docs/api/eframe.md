# API index: `eframe` 0.35.0

GENERATED FILE — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`. Extracted from rustdoc JSON of the exact crate source in the local cargo registry, so every signature and line number below is real.

**If a name is not in this file, it does not exist in our version of the crate.** Grep here before writing egui code from memory.

Items are listed under the path callers actually write (the public re-export, e.g. `egui::Panel`, `egui::Color32`), not where they happen to be defined. Citations point into the crate that owns the item, so a type `egui` re-exports from `epaint` cites `epaint-0.35.0/src/…`.

## `eframe`

### `APP_KEY` (constant) — `eframe-0.35.0/src/epi.rs:978`

[`Storage`] key used for app

### `EframePumpStatus` (enum) — `eframe-0.35.0/src/native/run.rs:568`

Either an exit code or a [`ControlFlow`] from the [`ActiveEventLoop`].

Variants:

- `EframePumpStatus::Continue` — The final state of the [`ControlFlow`] after all events have been dispatched
- `EframePumpStatus::Exit` — The exit code for the application

### `Error` (enum) — `eframe-0.35.0/src/lib.rs:504`

The different problems that can occur when trying to run `eframe`.

Variants:

- `Error::AppCreation` — Something went wrong in user code when creating the app.
- `Error::Winit` — An error from [`winit`].
- `Error::WinitEventLoop` — An error from [`winit::event_loop::EventLoop`].
- `Error::Glutin` — An error from [`glutin`] when using [`glow`].
- `Error::NoGlutinConfigs` — An error from [`glutin`] when using [`glow`].
- `Error::OpenGL` — An error from [`glutin`] when using [`glow`].

Implements: `Debug`, `Display`, `Error`, `From<Error>`, `From<EventLoopError>`, `From<OsError>`, `From<PainterError>`

### `Renderer` (enum) — `eframe-0.35.0/src/epi.rs:582`

What rendering backend to use.

Variants:

- `Renderer::Glow` — Use [`egui_glow`] renderer for [`glow`](https://github.com/grovesNL/glow).

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Display`, `Eq`, `FromStr`, `PartialEq`, `StructuralPartialEq`

### `UserEvent` (enum) — `eframe-0.35.0/src/native/winit_integration.rs:52`

The custom even `eframe` uses with the [`winit`] event loop.

Variants:

- `UserEvent::RequestRepaint` — A repaint is requested.
- `UserEvent::AccessKitActionRequest` — A request related to [`accesskit`](https://accesskit.dev/).

Implements: `ApplicationHandler<UserEvent>`, `Debug`, `From<Event>`

### `WebGlContextOption` (enum) — `eframe-0.35.0/src/epi.rs:559`

WebGL Context options

Variants:

- `WebGlContextOption::WebGl1` — Force Use WebGL1.
- `WebGlContextOption::WebGl2` — Force use WebGL2.
- `WebGlContextOption::BestFirst` — Use WebGL2 first.
- `WebGlContextOption::CompatibilityFirst` — Use WebGL1 first

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `Hash`, `PartialEq`, `StructuralPartialEq`

### `create_native` — `eframe-0.35.0/src/lib.rs:376`

```rust
fn create_native(app_name: &str, native_options: NativeOptions, app_creator: AppCreator<'a>, event_loop: &EventLoop<UserEvent>) -> EframeWinitApplication<'a>
```

Provides a proxy for your native eframe application to run on your own event loop.

### `run_native` — `eframe-0.35.0/src/lib.rs:288`

```rust
fn run_native(app_name: &str, native_options: NativeOptions, app_creator: AppCreator<'_>) -> Result
```

This is how you start a native (desktop) app.

### `run_native_ext` — `eframe-0.35.0/src/lib.rs:306`

```rust
fn run_native_ext(app_name: &str, native_options: NativeOptions, egui_ctx: Option<Context>, app_creator: AppCreator<'_>) -> Result
```

Like [`run_native`], but lets you supply a pre-existing [`egui::Context`].

### `run_ui_native` — `eframe-0.35.0/src/lib.rs:478`

```rust
fn run_ui_native(app_name: &str, native_options: NativeOptions, ui_fun: impl FnMut(&mut Ui, &mut Frame) + 'static) -> Result
```

The simplest way to get started when writing a native app.

### `CreationContext` (struct) — `eframe-0.35.0/src/epi.rs:53`

Data that is passed to [`AppCreator`] that can be used to setup and initialize your app.

Public fields:

- `egui_ctx: Context` — The egui Context.
- `integration_info: IntegrationInfo` — Information about the surrounding environment.
- `storage: Option<&'s dyn Storage>` — You can use the storage to restore app state(requires the "persistence" feature).
- `gl: Option<Arc<Context>>` — The [`glow::Context`] allows you to initialize OpenGL resources (e.g. shaders) that you m…
- `get_proc_address: Option<Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>>` — The `get_proc_address` wrapper of underlying GL context

Methods:

- `fn winit_window(&self) -> Option<&Arc<Window>>` — `eframe-0.35.0/src/epi.rs:144`
  Access to the root [`winit::window::Window`].

Implements: `HasDisplayHandle`, `HasWindowHandle`

### `EframeWinitApplication` (struct) — `eframe-0.35.0/src/native/run.rs:480`

A proxy to the eframe application that implements [`ApplicationHandler`].

Methods:

- `fn pump_eframe_app(&mut self, event_loop: &mut EventLoop<UserEvent>, timeout: Option<Duration>) -> EframePumpStatus` — `eframe-0.35.0/src/native/run.rs:550`
  Pump the `EventLoop` to check for and dispatch pending events to this application.

Implements: `ApplicationHandler<UserEvent>`

### `Frame` (struct) — `eframe-0.35.0/src/epi.rs:655`

Represents the surroundings of your app.

Methods:

- `fn gl(&self) -> Option<&Arc<Context>>` — `eframe-0.35.0/src/epi.rs:777`
  A reference to the underlying [`glow`] (OpenGL) context.
- `fn info(&self) -> &IntegrationInfo` — `eframe-0.35.0/src/epi.rs:742`
  Information about the integration.
- `fn is_web(&self) -> bool` — `eframe-0.35.0/src/epi.rs:737`
  True if you are in a web environment.
- `fn register_native_glow_texture(&mut self, native: Texture) -> TextureId` — `eframe-0.35.0/src/epi.rs:786`
  Register your own [`glow::Texture`], and then you can use the returned [`egui::TextureId`] to render your tex…
- `fn storage(&self) -> Option<&dyn Storage>` — `eframe-0.35.0/src/epi.rs:747`
  A place where you can store custom data in a way that persists when you restart the app.
- `fn storage_mut(&mut self) -> Option<&mut dyn Storage + 'static>` — `eframe-0.35.0/src/epi.rs:752`
  A place where you can store custom data in a way that persists when you restart the app.
- `fn winit_window(&self) -> Option<&Arc<Window>>` — `eframe-0.35.0/src/epi.rs:760`
  Access to the current [`winit::window::Window`] (i.e. the one the active viewport is rendered to).

Implements: `HasDisplayHandle`, `HasWindowHandle`

### `IntegrationInfo` (struct) — `eframe-0.35.0/src/epi.rs:892`

Information about the integration passed to the use app each frame.

Public fields:

- `cpu_usage: Option<f32>` — Seconds of cpu usage (in seconds) on the previous frame.

Implements: `Clone`, `Debug`

### `NativeOptions` (struct) — `eframe-0.35.0/src/epi.rs:290`

Options controlling the behavior of a native window.

Public fields:

- `viewport: ViewportBuilder` — Controls the native window of the root viewport.
- `multisampling: u16` — Set the level of the multisampling anti-aliasing (MSAA).
- `depth_buffer: u8` — Sets the number of bits in the depth buffer.
- `stencil_buffer: u8` — Sets the number of bits in the stencil buffer.
- `renderer: Renderer` — What rendering backend to use.
- `run_and_return: bool` — This controls what happens when you close the main eframe window.
- `event_loop_builder: Option<EventLoopBuilderHook>` — Hook into the building of an event loop before it is run.
- `window_builder: Option<WindowBuilderHook>` — Hook into the building of a window.
- `centered: bool` — On desktop: make the window position to be centered at initialization.
- `glow_options: GlowConfiguration` — Configures glow instance.
- `persist_window: bool` — Controls whether or not the native window position and size will be persisted (only if th…
- `persistence_path: Option<PathBuf>` — The folder where `eframe` will store the app state. If not set, eframe will use a default…
- `dithering: bool` — Controls whether to apply dithering to minimize banding artifacts.

Implements: `Clone`, `Default`

### `App` (trait) — `eframe-0.35.0/src/epi.rs:152`

Implement this trait to write apps that can be compiled for both web/wasm and desktop/native using [`eframe`](https://github.com/emilk/egui/tree/main/crates/eframe).

Required/provided items:

- `fn logic(&mut self, ctx: &Context, frame: &mut Frame)` — `eframe-0.35.0/src/epi.rs:161`
  Called once before each call to [`Self::ui`], and additionally also called when the UI is hidden, but [`egui:…
- `fn ui(&mut self, ui: &mut Ui, frame: &mut Frame)` — `eframe-0.35.0/src/epi.rs:176`
  Called each time the UI needs repainting, which may be many times per second.
- `fn save(&mut self, _storage: &mut dyn Storage)` — `eframe-0.35.0/src/epi.rs:206`
  Called on shutdown, and perhaps at regular intervals. Allows you to save state.
- `fn on_exit(&mut self, _gl: Option<&Context>)` — `eframe-0.35.0/src/epi.rs:216`
  Called once on shutdown, after [`Self::save`].
- `fn auto_save_interval(&self) -> Duration` — `eframe-0.35.0/src/epi.rs:228`
  Time between automatic calls to [`Self::save`]
- `fn clear_color(&self, _visuals: &Visuals) -> [f32; 4]` — `eframe-0.35.0/src/epi.rs:242`
  Background color values for the app, e.g. what is sent to `gl.clearColor`.
- `fn persist_egui_memory(&self) -> bool` — `eframe-0.35.0/src/epi.rs:253`
  Controls whether or not the egui memory (window positions etc) will be persisted (only if the "persistence" f…
- `fn raw_input_hook(&mut self, _ctx: &Context, _raw_input: &mut RawInput)` — `eframe-0.35.0/src/epi.rs:273`
  A hook for manipulating or filtering raw input before it is processed by [`Self::ui`].

### `Storage` (trait) — `eframe-0.35.0/src/epi.rs:938`

A place where you can store custom data in a way that persists when you restart the app.

Required/provided items:

- `fn get_string(&self, key: &str) -> Option<String>` — `eframe-0.35.0/src/epi.rs:940`
  Get the value for the given key.
- `fn set_string(&mut self, key: &str, value: String)` — `eframe-0.35.0/src/epi.rs:943`
  Set the value for the given key.
- `fn remove_string(&mut self, key: &str)` — `eframe-0.35.0/src/epi.rs:946`
  Remove a given key.
- `fn flush(&mut self)` — `eframe-0.35.0/src/epi.rs:949`
  write-to-disk or similar

### `AppCreator` (type_alias) — `eframe-0.35.0/src/epi.rs:49`

This is how your app is created.

### `EventLoopBuilderHook` (type_alias) — `eframe-0.35.0/src/epi.rs:34`

Hook into the building of an event loop before it is run

### `Result` (type_alias) — `eframe-0.35.0/src/lib.rs:617`

Short for `Result<T, eframe::Error>`.

### `WindowBuilderHook` (type_alias) — `eframe-0.35.0/src/epi.rs:42`

Hook into the building of a the native window.


## `eframe::icon_data`

### `from_png_bytes` — `eframe-0.35.0/src/icon_data.rs:24`

```rust
fn from_png_bytes(png_bytes: &[u8]) -> Result<IconData, ImageError>
```

Load the contents of .png file.

### `IconDataExt` (trait) — `eframe-0.35.0/src/icon_data.rs:6`

Helpers for working with [`IconData`].

Required/provided items:

- `fn to_image(&self) -> Result<RgbaImage, String>` — `eframe-0.35.0/src/icon_data.rs:11`
  Convert into [`image::RgbaImage`]
- `fn to_png_bytes(&self) -> Result<Vec<u8>, String>` — `eframe-0.35.0/src/icon_data.rs:17`
  Encode as PNG.


