# Module: src/shell

## Purpose
The program's main window: ONE `eframe::run_native` that hosts the launcher and the studio as
screens and stays open while the user switches between them. It owns everything about the
window; the screens own only their content.

## Architecture
```text
startup::run_native (src/startup/): startup prompts (own windows) -> shell::run(ShellConfig) -> ShellExit
AppShell (mod.rs, eframe::App)
  logic(): poll workers (launcher entry, studio load, studio teardown, update check,
           incoming-images take -> ExternalOpen) + root close_requested -> on_event(OsClose)
  ui():    geometry, Windows first-frame maximize, draw the active screen, UI exits,
           then drain_studio_outputs (the running studio's StudioOutput queue)
  raw_input_hook(): TouchRouter::route (ms_input::touch) before egui's pointer state
  dispatch(event) -> resolve_open_event (pending_open) -> screen::on_event(kind, event)
                  -> Transition { next, effect, cancel_os_close }
                  -> run_effect (takes the data from the active screen) -> enter_screen on a switch
screens:  PreparingLauncher (worker: ensure_model_dirs + ms_launcher::prepare_entry)
          Launcher (ms_launcher::LauncherApp)
          Studio (studio/: Loading -> Loaded | LoadFailed -> Running(StudioApp) | Failed)
          Closing (closing.rs: worker runs StudioApp::shutdown_blocking + scratch release)
          Exiting (the shell sent the root Close)
```
`AppShell` keeps the pure `ScreenKind` (decisions) next to `Active` (the screen's data). Every
transition goes through `screen::on_event`; the shell only executes the returned `Effect`.
Events and effects carry no worker data: what a worker delivered stays in the screen that
received it until the effect consumes it — the prepared entry in `LauncherPrepare::Ready`
(`ShowLauncher`), the loaded project in the studio's `Loaded` phase (`StartStudio`, or dropped
by `into_parts` on `Shutdown` when a close arrived during the load), the load error in
`LoadFailed` (`ShowLoadError`).

## Files and submodules
- `mod.rs`: `run`, `ShellConfig`, `InitialScreen`, `AppShell` (creator, `logic`, `ui`,
  `raw_input_hook`, `on_exit`), the PreparingLauncher worker and screen (`LauncherPrepare`), the
  cached update notification, `resolve_open_event` (the `pending_open` bookkeeping, a free
  function). Tests: `resolve_open_event` rows and windowless `AppShell` dispatch / poll runs on
  `egui::Context::default()` (screens that need no real launcher entry or `StudioApp`).
- `screen.rs`: the pure transition table (`on_event`, exhaustive OS-close policy
  `on_os_close`), `ShellExit`, per-screen style / minimum size. Unit-tested row by row.
- `style.rs`: `StyleScreen`, `style_for`, `apply` — the complete global style per screen, built
  from scratch. Compiled on wasm too (the web entry uses it).
- `chapter_load.rs`: `load_chapter` / `has_unsaved_staging` — the ONE owner of "resume the
  unsaved session?" (a `{chapter}_unsaved` folder exists -> `load_resume_unsaved`, else `load`).
  Used by `studio/open.rs` and the web entry; compiled on wasm too.
- `target.rs`: what the window is asked to open — `StartupTarget` (`Project` or
  `Images(ImageSources)`, + window title), `ImageSources` (ordered files, already-rejected
  `SkippedSource`s, Android inbox batches; `merged` concatenates), `received_count`, and the one
  `release_inbox_batches_blocking` (workers only). Shared by startup routing (`src/startup/routing.rs`),
  `screen.rs`, `incoming.rs` and `studio/`.
- `studio/`: the studio screen, one directory module.
  - `mod.rs`: `StudioScreen` and its `Phase` (loading with progress / confirmation / Cancel,
    loaded, load-failed, error, cancelled, running), `LoadResult`, `StudioExit`, `into_parts`,
    `take_outputs` / `reload_exit` (the running studio's queue and the reload's session check).
    Tests: the screen over a real open worker (large-set confirm / cancel / deferred close),
    settling effects, a lost worker.
  - `open.rs`: the open worker (`spawn_open_thread` -> `OpenWorker`, `WorkerLink`, `OpenEvent`,
    `OpenOutcome`: scratch reservation, `chapter_load::load_chapter`, or for an image
    set the size estimate, the large-set question and `open_image_set` with
    `OpenEvent::Progress` and a shared cancel flag). After a successful load the worker also
    reads the studio's own inputs (`crate::studio::StudioBootstrap::build_blocking`:
    user config, hotkey overrides, settings / typing / spellcheck seeds, staging probe) and
    delivers them with the project (`LoadedStudio`), so `StudioApp::new` runs in a frame without
    I/O. `open_target` (tested directly) stops before that read: `OpenedTarget::into_outcome`
    adds it.
  - `session.rs`: `StudioSession` (`Project { dir }` / `ImageSet { sources, scratch }`), the
    shell half of a session and the one shell-side owner of its kind: `from_target`,
    `is_image_set`, `reloadable`, `log_open`, `release` (scratch delete, inbox batches). The open
    worker decides the kind once: `OpenOutcome.session` and the loaded `ProjectData` agree
    (`is_image_set() == is_single_image()`, pinned by `open.rs` tests). Before the worker
    delivers, the screen asks the target's kind; no other shell field duplicates it.
  - `ui.rs`: `draw_loading_screen` (`LoadingView`, `LoadingAction`), `draw_error_screen`.
- `gl.rs`: `install` — the `egui-shader-layers` glow backend, from the app creator (`on_exit`
  destroys it). Native-gated with the shell host; the web entry installs its own.
- `incoming.rs`: external image delivery (Android Share / "Open with", `ms_platform::incoming`):
  `sources_from_batches` (batches -> one ordered `ImageSources`, Kotlin skips as localized
  `SkippedSource`s), the `incoming-take` worker (`spawn_take`), `cold_start_target` (pre-window,
  for `startup::run_native`), `discard_target` (log + release the inbox batches on a worker).
- `closing.rs`: `StudioShutdownJob` (teardown worker, `poll`, `finish_blocking`),
  `ClosingScreen` (spinner, or the failure + Continue), `teardown_blocking`.

## Contracts and invariants
- **No blocking work on the GUI thread at a switch.** Launcher entry, studio load and studio
  teardown each run on a named worker; the GUI thread only polls. `StudioApp` moves whole
  (`Box<StudioApp>`, `Send`-guarded in `src/studio/shutdown.rs`) onto the teardown worker, because
  dropping it joins threads. Blocking teardown on the GUI thread happens only in `on_exit` (eframe
  ended the window without the shell's consent) and when the teardown thread cannot be spawned.
- **Root close policy** (`screen.rs` header): PreparingLauncher / Launcher / failed load allow;
  Loading cancels and closes after the worker delivered (it may be mid-write); Running cancels and
  asks `StudioApp::request_os_close`; Closing cancels and becomes "exit after teardown"; Exiting
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
- **One outbound channel from the studio**: everything a running `StudioApp` asks of the window
  (`crate::studio::StudioOutput`: `Title`, `Close`, `IncomingDeclined`, `ReloadRequested`) is drained
  by `AppShell::drain_studio_outputs` through `StudioScreen::take_outputs` and applied in the
  queue's order (`studio/studio_output.rs::collect_outputs`: a close suppresses a same-drain reload,
  a close resolves `pending_open` before a decline). The drain runs after every studio draw (same
  frame as the decision), in `poll_workers` (outputs decided while no draw ran) and right after
  `Effect::RequestOsClose` handed the OS close over (so a minimized window still closes). A
  `Title` goes through the same title path as a screen switch (`apply_title`, `studio_title`).
  Inbound calls stay direct: `request_os_close`, `request_close_for_incoming`.
- **Startup prompts run once** (in `startup::run_native`, before the window); the startup update check runs
  once, on the first launcher entry, and its answer is handed to every later launcher entry.
- **AI model folders** (`ms_config::ensure_model_dirs`) are created by the launcher-prepare worker
  on every launcher entry. A failure is logged as a warning and the launcher still opens.
- **Image-set scratch**: reserved by the open worker under `ms_config::single_image::scratch_base()`
  (a failure, like an open/decode failure, is the studio error screen with `SingleImageError::user_message`
  and the usual "Exit to launcher" / "Exit"; a CLI start therefore exits with code 0),
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
- **External delivery** (images another app hands over; `screen.rs` header): `AppShell::new`
  registers the repaint waker; `logic` polls `incoming::has_pending()` (atomic, no JNI) and takes
  on the `incoming-take` worker; the target is dispatched as `ExternalOpen`, after folding in any
  target still pending (`merge_open`: image sets concatenate in arrival order). Launcher: opens at
  once. PreparingLauncher / Loading: `QueueOpen` into `AppShell::pending_open`, re-fed after
  `ShowLauncher` / `StartStudio`. Running: `AskStudioToOpen` keeps the target in `pending_open`
  and calls `StudioApp::request_close_for_incoming` (dirty: the unsaved-changes flow; clean: a
  one-line confirmation); the studio's `StudioClose(OpenIncoming)` is mapped to
  `StudioCloseForOpen(pending_open)` -> Closing{Open} -> Loading. Failed load: replaced.
  Closing: continues into the open (newest intent wins). Exiting: dropped. A target the window
  will not open (declined — `StudioOutput::IncomingDeclined` —, the studio closed for another reason, the window exits, a refused
  ask) is dropped by `incoming::discard_target`, which releases its inbox batches on a worker.
  Cold start: `startup::run_native` takes the batches pre-window; they become `InitialScreen::Studio` when
  there is no CLI target. A failed take is logged and shown once in a small window.
- **Canvas touch routing**: the shell owns the window's ONE `ms_input::touch::TouchRouter`
  (created once, lives across every screen) and feeds it from `raw_input_hook`, the only point
  that sees `RawInput` before egui builds its pointer state. On iff
  `ms_platform::PlatformFeature::CanvasTouchGestures` is available (off on Windows: pen and
  touch are indistinguishable there) AND the user setting `General.touch_gestures` is on
  (`ms_config::touch_gestures`, re-read every frame and handed to `TouchRouter::set_enabled`,
  which switches at a safe frame start; the router is never recreated). Screens only publish surfaces and take outputs; the
  launcher publishes none, so it is unaffected.
- Android: one `run_native`; the fork's secondary-window policy is set before it and re-synced at
  creation and every switch. Nothing here relies on the winit fork re-emitting `Resumed` for a
  second event loop (`dev-docs/ANDROID_PORT.md`).
- **Resume unsaved**: decided only by `chapter_load` from the filesystem; no caller passes a
  flag. The launcher's "open fresh" is the deletion of the staging folder before it opens.
- Web: only `style.rs` and `chapter_load.rs` are shared; the web entry keeps its own launcher -> editor swap (the full
  shell port is deferred, see `src/web_entry.rs`).

## Editing map
- Where the window goes after an event, or the close policy: `screen.rs` (+ its tests), then the
  effect in `mod.rs::run_effect`.
- Window attributes per screen (title, minimum size, style): `screen.rs` (`style_screen`,
  `min_inner_size`), `mod.rs::enter_screen`, `style.rs`.
- What can be opened (the target types, the window title of a target): `target.rs`; what the
  studio does with it (resume-unsaved: `chapter_load.rs`; image set, progress / confirmation, error screen):
  `studio/` (`open.rs` for the load, `ui.rs` for the screens, `mod.rs` for the phases).
- Teardown order: `src/studio/shutdown.rs`; how it runs and what the user sees: `closing.rs`.
- Startup prompts and the post-window update: `src/startup/` (`routing.rs`: `resolve_start`,
  `resolve_cli_images`, `run_launcher_requested_update`; `prompts.rs`).
- Finger gestures on canvases: the router and its contract live in
  `crates/ms-input/src/touch/`; the platform gate in `ms_platform` `features.rs`; the
  user switch in `ms_config::touch_gestures`; the shell only wires `raw_input_hook`.
- Images other apps hand over: where they go per screen — `screen.rs` (`ExternalOpen` rows,
  `merge_open`); taking, mapping, dropping — `incoming.rs`; `pending_open` and the polling —
  `mod.rs`; the running studio's question — `src/studio/incoming_open.rs`.
