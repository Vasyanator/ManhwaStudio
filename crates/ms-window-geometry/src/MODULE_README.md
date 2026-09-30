# Module: crates/ms-window-geometry/src

## Purpose
Owns WHERE the program's OS window opens and where it is left: the user's primary-monitor
choice, the last known geometry of the studio window, and the plumbing that keeps both in
the self-versioned `Window` section of `user_config.json`. Re-exported by the binary as
`crate::window_geometry` (`src/main.rs`).

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
binary (`main.rs`, `studio_bootstrap.rs`, the launcher) and the settings UI.

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

## Contracts and invariants
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
- **Owners.** The studio owns the geometry: `StudioBootstrapApp` (`src/studio_bootstrap.rs`)
  holds the `WindowGeometryTracker` from the first frame and flushes it in `on_exit`. The
  launcher only follows the monitor choice (`SizePolicy::KeepDefault`) and never persists
  geometry. A geometry sample is taken only while the window is neither maximized, minimized
  nor fullscreen; a write touches only its own fields of the section.
- **Windows first-frame maximize.** On Windows the restored maximized state is applied as a
  first-frame `ViewportCommand::Maximized`, gated on the persisted `Window.maximized` flag
  (default `true`), by `StudioBootstrapApp`.
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
