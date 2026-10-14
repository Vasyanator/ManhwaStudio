# Module: src/shell

## Purpose
The program's main window: ONE `eframe::run_native` that hosts the launcher and the studio as
screens and stays open while the user switches between them. It owns everything about the
window; the screens own only their content.

## Architecture
```text
run_main (src/lib.rs): startup prompts (own windows) -> shell::run(ShellConfig) -> ShellExit
AppShell (mod.rs, eframe::App)
  logic(): poll workers (launcher entry, studio load, studio teardown, update check)
           + root close_requested -> screen::on_event(OsClose)
  ui():    geometry, Windows first-frame maximize, draw the active screen, UI exits
  dispatch(event) -> screen::on_event(kind, event) -> Transition { next, effect, cancel_os_close }
                  -> run_effect (moves data between screens) -> enter_screen on a switch
screens:  PreparingLauncher (worker: ensure_model_dirs + ms_launcher::prepare_entry)
          Launcher (ms_launcher::LauncherApp)
          Studio (studio.rs: Loading -> Failed | Running(MangaApp))
          Closing (closing.rs: worker runs MangaApp::shutdown_blocking + scratch release)
          Exiting (the shell sent the root Close)
```
`AppShell` keeps the pure `ScreenKind` (decisions) next to `Active` (the screen's data). Every
transition goes through `screen::on_event`; the shell only executes the returned `Effect`.

## Files and submodules
- `mod.rs`: `run`, `ShellConfig`, `InitialScreen`, `AppShell` (creator, `logic`, `ui`,
  `on_exit`), the PreparingLauncher worker and screen, the cached update notification.
- `screen.rs`: the pure transition table (`on_event`, exhaustive OS-close policy
  `on_os_close`), `ShellExit`, per-screen style / minimum size. Unit-tested row by row.
- `style.rs`: `StyleScreen`, `style_for`, `apply` — the complete global style per screen, built
  from scratch. The only part compiled on wasm (the web entry uses it).
- `studio.rs`: `StartupTarget` (`Project` or `Images(ImageSources)`, + window title),
  `ImageSources` (ordered files, already-rejected `SkippedSource`s, Android inbox batches;
  `merged` concatenates), `StudioOpenRequest` (+ `release`: scratch delete, inbox batches), the
  open worker (`spawn_open_thread` -> `OpenWorker`: scratch reservation, unsaved detection,
  `ProjectData::load*`, or for an image set the size estimate, the large-set question and
  `open_image_set` with `OpenEvent::Progress` and a shared cancel flag), `StudioScreen` (loading
  with progress / confirmation / Cancel, error, cancelled, running), `install_shader_layers`.
- `closing.rs`: `StudioShutdownJob` (teardown worker, `poll`, `finish_blocking`),
  `ClosingScreen` (spinner, or the failure + Continue), `teardown_blocking`.

## Contracts and invariants
- **No blocking work on the GUI thread at a switch.** Launcher entry, studio load and studio
  teardown each run on a named worker; the GUI thread only polls. `MangaApp` moves whole
  (`Box<MangaApp>`, `Send`-guarded in `src/app/shutdown.rs`) onto the teardown worker, because
  dropping it joins threads. Blocking teardown on the GUI thread happens only in `on_exit` (eframe
  ended the window without the shell's consent) and when the teardown thread cannot be spawned.
- **Root close policy** (`screen.rs` header): PreparingLauncher / Launcher / failed load allow;
  Loading cancels and closes after the worker delivered (it may be mid-write); Running cancels and
  asks `MangaApp::request_os_close`; Closing cancels and becomes "exit after teardown"; Exiting
  allows (a `Close` the shell sent comes back as `close_requested` next frame — never cancel it).
  The decision is taken in `logic`, so it also works while the window is minimized
  (eframe-0.36.2/src/native/epi_integration.rs:305-310, :340-346).
- **Every screen switch** rebuilds and installs the complete style (`style::apply`; never compare
  `Style`s to skip it — `number_formatter` compares by pointer; PreparingLauncher installs the
  launcher style itself, since it can be the first screen; Closing / Exiting keep the previous
  one), sets the title and the minimum
  size (launcher 980x680, studio 900x600) and re-syncs the secondary-window policy
  (`ms_window_geometry::sync_secondary_window_policy`, Android). Entering the launcher starts the
  pending storage reconciliation; a studio that finished loading starts it too (`--project`).
- **Created once per process**: UI fonts (`ui_fonts::install(Tier::Full)`), UI scale, the
  shader-layer GL backend (destroyed in `on_exit`), `zoom_with_keyboard = false`, the
  `WindowGeometryTracker` (the `Window` config section describes this window), the window
  identity (`ms_window_geometry::identity`). A screen never closes, maximizes, titles or styles
  the window.
- **Startup prompts run once** (in `run_main`, before the window); the startup update check runs
  once, on the first launcher entry, and its answer is handed to every later launcher entry.
- **AI model folders** (`ms_config::ensure_model_dirs`) are created by the launcher-prepare worker
  on every launcher entry. A failure is logged as a warning and the launcher still opens — a
  deliberate (accepted) change from the old startup, where a failure aborted the process.
- **Image-set scratch**: reserved by the open worker (a failure is the studio error screen),
  owned by `StudioScreen`, released by the teardown worker after the app was dropped, together
  with the set's Android inbox batches.
- **Image-set open on the loading screen**: a set of 2+ files above the soft limits
  (`needs_size_confirmation`) asks first; the worker blocks on the answer, and a dropped answer
  channel is a cancel. A user Cancel ends as a cancelled load: the table's `StudioLoadFailed`
  row enters the Failed phase and the screen exits with `FailedBack` at once (launcher +
  scratch release). A deferred OS close cancels an image-set open itself (question answered
  "no", cancel flag raised) so the close never waits on the user; it is applied from `ui`, so a
  minimized window answers once it is drawn again. A one-file set opens with no estimate and no
  question, exactly as the classic single-image mode.
- **Bad image files are reported in one place**: the CLI rejects a start only when no path is a
  file (startup error dialog), the launcher stays only when no picked file is usable; otherwise
  every path goes to the open worker, which lists the unusable ones as skipped.
- Android: one `run_native`; the fork's secondary-window policy is set before it and re-synced at
  creation and every switch. Nothing here relies on the winit fork re-emitting `Resumed` for a
  second event loop (`dev-docs/ANDROID_PORT.md`).
- Web: only `style.rs` is shared; the web entry keeps its own launcher -> editor swap (the full
  shell port is deferred, see `src/web_entry.rs`).

## Editing map
- Where the window goes after an event, or the close policy: `screen.rs` (+ its tests), then the
  effect in `mod.rs::run_effect`.
- Window attributes per screen (title, minimum size, style): `screen.rs` (`style_screen`,
  `min_inner_size`), `mod.rs::enter_screen`, `style.rs`.
- What the studio opens and how (unsaved detection, image set, progress / confirmation, error
  screen): `studio.rs`.
- Teardown order: `src/app/shutdown.rs`; how it runs and what the user sees: `closing.rs`.
- Startup prompts and the post-window update: `src/lib.rs` (`resolve_start`,
  `resolve_cli_images`, `run_launcher_requested_update`).
