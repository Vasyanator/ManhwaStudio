# Module: crates/ms-window-geometry/src

## Purpose
Owns WHERE the program's OS window opens and where it is left: the user's primary-monitor
choice, the last known geometry of the main window (the one window hosting the launcher and the
studio), and the plumbing that keeps both in
the self-versioned `Window` section of `user_config.json`. Re-exported by the binary as
`crate::window_geometry` (`src/lib.rs`).

Native-only: winit windows and OS monitors do not exist in the web build, so the binary
depends on this crate from its `cfg(not(target_arch = "wasm32"))` block.

## Architecture
```text
startup (no window yet)          runtime (window exists)
  load_window_settings()           refresh_monitors(&Window)  -> MONITOR_SNAPSHOT
  plan_startup_placement()         WindowGeometryTracker::observe(ctx, window)
  apply_placement(ViewportBuilder)         |  sample_geometry -> GeometrySnapshot
                                           v
                              ms_config::config_saver::ConfigSaver (writer thread)
                                           |
                                           v
                              update_window_section -> user_config.json
```

Layer position: `ms-config` (+ its `config_saver`) <- **`ms-window-geometry`** <- the
binary (the main window, `src/shell/`), `ms-launcher` (window identity) and the settings UI.

This crate is the project's ONLY direct user of `winit`: egui/eframe expose no monitor
list, only the raw window handle (`CreationContext::winit_window` / `Frame::winit_window`),
whose type comes from `winit` itself. The version must therefore stay the one eframe pulls
in, or the two `Window` types stop unifying.

## Files and submodules
- `lib.rs`: the whole crate — `MonitorKey`, `WindowRect`, `WindowSettings`,
  `MonitorSnapshot`, `GeometrySnapshot`, `WindowGeometryTracker`, the startup planners
  (`plan_startup_placement` / `apply_placement`), the monitor resolution
  (`resolve_monitor` / `largest_monitor_index` / `should_relocate`) and the persistence
  path (`update_window_section` / `persist_geometry` / `spawn_geometry_saver`).
- `android_app.rs` (`cfg(target_os = "android")` only): the one process-global owner of the
  Android `AndroidApp` handle (`install` / `current`, re-exported type `AndroidApp`). It lives
  here because this crate is the only direct `winit` user and already sits under both the
  binary and `ms-launcher`. `android_main` (`src/android_entry.rs`) installs the handle, and
  REPLACES it when android-activity calls `android_main` again for a re-created Activity;
  every `eframe::NativeOptions` in the process sets `android_app: android_app::current()`.
  It also applies `General.secondary_windows` (`ms_config::SecondaryWindows`) to the forked
  winit (`set_secondary_window_policy`: resolved for Android, `Separate` -> `Native`,
  otherwise `Embedded`; the fork's device auto-detection is never used) and exposes the fork's
  runtime gate (`native_windows_available`). The main window (`src/shell/mod.rs`) sets the
  policy from the live `ms_config::secondary_windows` value right before its one
  `eframe::run_native` (eframe reads it when it creates the event loop), and calls the crate-root
  `sync_secondary_window_policy(ctx, setting)` (a no-op off Android) at creation and on every
  screen switch: it sets the policy, then `ctx.set_embed_viewports(!native_windows_available())`,
  so the fork's full-screen fallback still wins; no I/O. The startup prompts keep whatever policy
  is set (they open no secondary viewports).
- `identity.rs`: the program's window identity — `MAIN_WINDOW_APP_ID` / `TEST_WINDOW_APP_ID`,
  `app_id(test_mode)`, `app_icon()` (embedded ICO, PNG fallback, decoded once per process) and
  `with_identity(builder, test_mode)`. Builds on every target that compiles this crate.

## Contracts and invariants
- **Window identity has one owner.** Every window grouped with the main window (the main
  window itself, launcher child windows) takes its app id and icon from `identity`; no other
  crate spells the main app id or decodes the icon. The prompt windows of `src/lib.rs` and the
  installer keep their own app ids and take only the icon (the installer still has its own
  decoder: known duplicate, to be switched to `identity::app_icon`).
- **Units.** Everything persisted is in *logical pixels* (physical / DPI scale), independent
  of the egui zoom factor. `MonitorKey` is the one exception: it stores what winit reports,
  i.e. physical pixels, plus the scale needed to convert. See the UNITS note in `lib.rs`.
- **Ordering.** A second winit `EventLoop` cannot be created, so no monitor list exists
  before the window does. Startup works off the STORED monitor rect; the live list is only
  used at runtime.
- **Durability.** Samples reach disk only through `ms_config::config_saver::ConfigSaver`.
  Once a sample is queued the saver is its last owner; `GeometrySnapshot::coalesce` folds
  field by field, where a `None` field means "not measurable", never "forget it".
- **Wayland.** The compositor owns placement there: no geometry is persisted, no relocation
  is attempted, and the settings UI says so instead of pretending to work.
- **Section version.** `WINDOW_SECTION_VERSION` is mirrored as a literal in `ms-config`'s
  default tree (this crate is native-only and cannot be referenced from there); a drift test
  in `lib.rs` keeps the two in step.
- **Owners.** The main window owns the geometry: `AppShell` (`src/shell/mod.rs`) restores it
  (`plan_startup_placement`), holds the `WindowGeometryTracker` from the first frame, whichever
  screen (launcher or studio) is shown, and flushes it in `on_exit`. A geometry sample is taken only while the window is neither maximized, minimized
  nor fullscreen; a write touches only its own fields of the section.
- **Windows first-frame maximize.** On Windows the restored maximized state is applied as a
  first-frame `ViewportCommand::Maximized`, gated on the persisted `Window.maximized` flag
  (default `true`), by `AppShell`.
- **No `ViewportBuilder::with_monitor`.** In egui-winit 0.36.2 it requests BORDERLESS
  FULLSCREEN on that monitor (`egui-winit-0.36.2/src/lib.rs:2003-2006`), so placement uses a
  stored position inside the monitor instead.
- Blocking work never runs on the GUI thread: `observe` only samples and hands the sample to
  the writer thread.

## Editing map
- To change what is stored in `user_config.json`, see `WindowSettings` +
  `update_window_section` and bump `WINDOW_SECTION_VERSION` (the drift test will point at
  the `ms-config` default tree).
- To change where the window opens, see `plan_startup_placement` / `apply_placement`.
- To change monitor identity or the degrade-to-largest rule, see `MonitorKey` /
  `resolve_monitor` / `largest_monitor_index`.
- To change the persistence cadence or retry behaviour, edit
  `ms-config`'s `config_saver`, not this crate.
