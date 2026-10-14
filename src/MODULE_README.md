# Module: src

## Purpose
The root package `manhwastudio_rs`: a library (`lib.rs`) holding the native startup flow
(`startup/`), the main window (`shell/`: one window hosting the launcher and the studio as
screens), the editor root (`StudioApp`, `studio.rs` + `studio/`), the Settings tab
(`tabs/settings/`), the web and Android entries and the crate-shim map, plus the thin binary
(`main.rs`) that only calls into it. Everything else — launcher, installer, settings panes, the
other tabs, canvas, models, widgets, config — lives in `crates/*` (layers: `ARCHITECTURE.md` §2).
This file is the map of the package: each child directory documents itself in its own
`MODULE_README.md`; this one holds only the root files, the shim map and the contracts that span
several child directories.

## Architecture
```text
main.rs -> lib.rs::native_main            android_entry.rs::android_main -> native_main
  -> startup/ (run_native: CLI, run mode, service flags, ProcessServices, prompts, resolve_start)
  -> shell/ (ONE eframe::run_native, AppShell)
       PreparingLauncher -> Launcher (ms_launcher) <-> Studio screen (shell/studio/)
       Studio screen: open worker (StartupTarget -> StudioSession + ProjectData + StudioBootstrap)
                      -> StudioApp (studio.rs) -> Closing screen (teardown worker)
  -> startup/ again after the window: the launcher-requested update window
StudioApp: PageResidency (studio/residency/) | StagingWriters | SessionMode (studio/session/)
           | Tabs (studio/tabs.rs) | CloseFlow | ViewSync | panel dock | outbound StudioOutput
web_main -> web_entry.rs (wasm: launcher -> StudioApp swap, no shell)
```
Startup decides the run mode and the first screen; the shell owns the window and every switch
between screens (workers for launcher entry, studio load and studio teardown); `StudioApp` owns
one opened session and talks to the window only through its `StudioOutput` queue
(`take_outputs`) and the two inbound calls `request_os_close` / `request_close_for_incoming`.

## Files and submodules
- `main.rs`: the binary's process entry only — the Windows `windows_subsystem` attribute and the
  native / wasm32 `fn main`, which call `native_main()` / `web_main()`.
- `lib.rs`: module tree, crate shims, `native_main` (runs `startup::run_native`, logs how the
  process ended) and `web_main`. Those two are the library's only public items; the shims are
  `pub(crate)`, so a shim no call site uses is an unused-import error and is removed. The library
  exists so a platform build that needs one (Android `cdylib`) reaches the same startup code; its
  crate type stays the default `rlib`.
- `startup/` (native-only): run modes, startup order, process-global seeds, service flags, the
  one-time prompt windows, start routing. See `startup/MODULE_README.md`.
- `shell/` (native, `style.rs` / `chapter_load.rs` also on wasm): the main window, its screens,
  `StartupTarget` (`target.rs`), the studio screen and open worker (`studio/`), teardown
  (`closing.rs`), external image delivery (`incoming.rs`), GL backend (`gl.rs`). See
  `shell/MODULE_README.md`.
- `studio.rs` + `studio/`: `StudioApp`, the editor root, and its owned parts (residency, staging
  writers, session strategy, tabs, close flow, view sync, prompts, bootstrap, teardown). See
  `studio/MODULE_README.md`; the session kinds in `studio/session/` and below.
- `tabs/`: the tab shim map (`crate::tabs::<tab>` -> `ms-tab-*` crates) and the Settings tab
  (`tabs/settings/`, the only tab compiled in this package). See `tabs/MODULE_README.md`.
- `args.rs`: the `clap` CLI contract — visible startup / update flags, the open target
  (`--project` XOR `--image` XOR a positional image path, clap group `open_target`), the
  update-check test override, environment-check and run-from-sources flags, hidden installer /
  update continuation flags — plus the pure flag-combination rules
  (`conflicting_installed_copy_flags`, `conflicting_image_flags`, `conflicting_project_flags`,
  `windows_only_flags`) and the image path pre-check. Consumed by `startup/mode.rs`.
- `update_check.rs` (native): the startup check for a newer GitHub release on a worker
  (comparison on `CARGO_PKG_VERSION`), started once by the shell on the first launcher entry.
- `web_entry.rs` (`wasm32`): the browser entry — in-memory storage, a generated demo chapter, a
  launcher -> `StudioApp` swap behind one `eframe::App`. It shares only `shell::style` and
  `shell::chapter_load` with the native window; the studio's outbound queue is drained and
  ignored there (logged).
- `android_entry.rs` (`cfg(target_os = "android")`): the unmangled `android_main(AndroidApp)`.
  Order is a contract: register the handle in `ms_window_geometry::android_app` (every
  `NativeOptions` reads `android_app` from there), refuse to start unless the Kotlin
  `AssetInstaller` installed THIS APK's bundled files into the runtime root (bundled stamp asset
  `ms_root.stamp` == `ms_root/.ms_assets_stamp`; names shared with `AssetInstaller.kt`, see
  `android/README.md`), init `rustls-platform-verifier` with the application context (fatal on
  failure: the verifier panics on first use otherwise), create `<internal data dir>/ms_root` and
  make it the working directory BEFORE any `ms_config` / `ms_fonts` call (the runtime root falls
  back to the working dir), then `native_main()`. Errors go to logcat and `android_main` returns.
  Logging: `android_logger` for the `log` facade; android-activity forwards stdout/stderr and
  panics to logcat; `runtime_log` writes `ms_root/last.log` as on desktop. A new
  `eframe::run_native` anywhere in the process must set
  `android_app: ms_window_geometry::android_app::current()` under the same cfg.
- `i18n_resolve.rs`: not a re-export — every caller names `ms_i18n::resolve_key` directly. The
  module only HOSTS the cross-crate tests guarding it: they assert against `ms-text-util`'s key
  sets and hold `ms_config::locale_store::GLOBAL_LOCALE_LOCK`, and `ms-i18n` sits below both, so
  this package is the lowest place that sees all three.
- `python_modules.zip`: bundled data, not source.

## Crate-shim map
`lib.rs` mounts extracted crates under the `crate::…` names the root package's call sites use;
`tabs/mod.rs` does the same for the tab crates (`tabs/MODULE_README.md`). Every crate documents
itself in `crates/<crate>/src/MODULE_README.md` (or `crates/<crate>/MODULE_README.md`); a crate
without a shim is named `ms_x::…` directly (`ms_tools`, `ms_onnx_runtime`, `ms_native_runtime`,
the other `ms_sysprobe` modules such as `ai_models`, `gpu_utils`, `paste_image`).

| `crate::` path | Crate (module) |
|---|---|
| `config`, `app_tab`, `locale_store` (native), `version_format` | `ms-config` |
| `models`, `page_view` | `ms-models` |
| `canvas` | `ms-canvas` |
| `project`, `project_scan` | `ms-project` |
| `page_ops` | `ms-page-ops` |
| `widgets`, `ui_fonts`, `bubble_status`, `input_manager_v2` | `ms-widgets` |
| `settings_shared`, `general_settings_panel`, `ai_backend_supervisor`, `storage_mode_job`, `tutorial` | `ms-settings-ui` |
| `launcher` | `ms-launcher` |
| `installer`, `venv_check` (native) | `ms-installer` |
| `window_geometry` (native) | `ms-window-geometry` |
| `runtime_log`, `trace`, `trace_log!` | `ms-log` |
| `backend_ipc` | `ms-backend-ipc` |
| `python_manager`, `ai_install_probe`, `hf_token` | `ms-sysprobe` |
| `memory_manager` | `ms-memory` |
| `storage` | `ms-storage` (`global`) |
| `text_punctuation`, `language` | `ms-text-util` |

`#[macro_use] extern crate ms_i18n` in `lib.rs` brings `t!` / `tf!` / `tp!` into scope crate-wide.

## Contracts and invariants
- **GUI thread.** Filesystem traversal, image decode, archive work, downloads, model probes,
  rendering, export, AI calls, saver barriers and worker joins run on workers, including every
  screen switch of the main window; the GUI thread polls and applies results.
- **Runtime paths** belong in the `config` crate (`crates/ms-config`): no hard-coded writable
  data, model, config, log or project paths in feature modules. `ProjectPaths` owns the chapter
  layout.
- **The application version has TWO forms** — a safety rule, not a preference. `MS_APP_VERSION`
  (composed by `build.rs`, e.g. `3.6.0+1cd9638-83-dirty`) is the HUMAN form: window title, version
  labels, `--version`, diagnostic logs. `CARGO_PKG_VERSION` is the MACHINE form: every comparison
  and every value another process parses — every caller of the one release comparator
  (`version_format::compare_release_versions` / `is_remote_newer`: `update_check.rs`,
  `ms-installer`'s `update.rs` / `utils.rs`). A suffix on a compared value makes a build outrank
  its own release tag. Where an extended string unavoidably crosses a process boundary (an
  installed copy probed with `--version`), reduce BOTH sides with `version_format::version_core`.
  The Python backend is never version-compared: compatibility is `PROTOCOL_VERSION` in `hello`.
  A library cannot read the binary's version (`MS_APP_VERSION` is a `rustc-env` of the ROOT crate
  only; `CARGO_PKG_VERSION` inside a crate is that crate's own), so `startup/` hands the pair to
  the crates that need it as `ms_installer::HostVersion`. `version_format` stays std-only: it is
  also `include!`d by `build.rs`, so the code the build script runs is the code `cargo test`
  covers.
- **`General.enabled_tabs`** keys are the stable English `AppTab::key()` ids, never localized
  (`dev-docs/i18n_exclusions.md` A3/B1). The field has NO reader. `merge_missing` adds keys but
  never removes, so an old user config may still carry legacy Russian tab labels as keys next to
  the English ids; they are inert and left on disk. A feature that starts reading `enabled_tabs`
  owns the one-time cleanup migration (keyed by `AppTab::key()`).
- **Global style** is a pure function of the active screen: the shell (and the web launcher ->
  editor swap) installs it ONLY through `shell::style::apply` (`ms_theme::install` of the launcher
  layer or the studio base), rebuilt from scratch on every switch; never a bare `set_theme`. The
  separate prompt windows use `ms_theme::apply`.
- **Fonts** are installed only through `ms_widgets::ui_fonts`, once per `run_native` creator, and
  only with `egui::Context::add_font`: `Context::set_fonts` would drop runtime-registered families
  (epaint panic); `Context::fonts*` must not run before the first frame or off the GUI thread.
- **Floating panels** are panel-dock tabs (`CollapsiblePanel` + `PanelTab`,
  `crates/ms-widgets/src/panel_dock/`), never a hand-rolled `Area + Frame::popup` or an
  `egui::Window` used as a panel.
- **Python and models.** Python discovery, command construction and spawning go only through
  `python_manager` (long-lived daemons through its managed spawn, a kill-on-close Job Object on
  Windows). App-managed model downloads go only through `ms_sysprobe::ai_models` and fail with
  explicit errors; library-managed caches (EasyOCR / Surya) are not redirected through it.
- **Memory.** Image cache retention uses `ms_memory` policy objects; cache owners keep pixels and
  texture handles local.
- **Shared model locks** are short-lived: snapshot, release, then render, save, decode, call
  hooks or process images. Canvas behaviour shared by tabs goes through `CanvasHooks` or typed
  canvas APIs, never a duplicated canvas state machine.
- **Explicit units** at public boundaries: page pixels, scene / screen / UV coordinates,
  width / height, row / column, RGBA / mask buffer lengths.
- Unsupported features return clear errors; no fake fallbacks, placeholder outputs or support
  inferred from filenames when typed metadata exists. Errors carry a user-facing status and a
  diagnostic log without secrets or large dumps.
- The Python UI under `old_or_test/` is not an architecture reference.

## Save / page-op / discard / exit choreography
The ordered writer sequences around every destructive point (save-to-project, page op, DISCARD,
failed discard, save-then-close, image-set close, teardown) are documented in
`studio/MODULE_README.md`, section "Save / page-op / discard / exit choreography".

## Parameter-clump consolidation: settled scope
Recorded here so it is not re-litigated. A 2026-07 multi-agent review of `src/` inventoried every
function threading a large loose-argument clump. Its principle stands: consolidate into an owned
`Copy` snapshot / context struct built AT the call boundary, never into a context that borrows a
whole tab or the whole `CanvasView` — most argument explosions in `crates/ms-tab-cleaning/src/`
are deliberate disjoint-borrow workarounds and need a durable STATE split, not a call-site bag.
Exemplars to copy: `PageView` (`crates/ms-tab-typing/src/tab/mesh_geometry.rs`, the
`(page_idx, image_rect, zoom)` triple) and the canvas' `BubbleMenuContext` /
`BubbleMenuOutcome` / `BubbleMenuCommand` (`crates/ms-canvas/src/types.rs`).
Deliberately left as they are (do not re-propose without new evidence; a wide signature is not by
itself a defect):
- `export_dispatch_ready` (`crates/ms-tab-typing/src/tab.rs`, three `bool`s): a pure,
  `#[must_use]`, documented and unit-tested predicate.
- The typing mask kernels — `flood_fill_mask_from_seed` (`crates/ms-tab-typing/src/mask.rs`) and
  the mask painters: owned-buffer worker kernels whose parameters have no natural grouping; the
  local `#[allow(clippy::too_many_arguments)]` carries its justification. The one carve-out worth
  taking opportunistically is `erase: bool` -> a `MaskStrokeMode` enum.
- Numeric kernels and rasterizers where the parameters ARE the algorithm: the SOR / Poisson / Lab
  kernels in `crates/ms-tab-cleaning/src/tools/gradient.rs`, the circle rasterizers in
  `ms-tools`' `mask_brush.rs`, the pure leaves of `mesh_geometry.rs`, `pack_aside_slots` and
  `reserve_canvas_page_frame` in `ms-canvas`, `evaluate_bubble_shape`'s scoring internals
  (`crates/ms-tab-typing/src/auto_typing.rs`), and the `shell/studio/open.rs` spawn.
- `TypingExportPageJob` and the render-request structs in
  `crates/ms-tab-typing/src/tab/render_store.rs`: already the target pattern.
- The `use super::*` style in typing's descendant modules follows from the documented
  descendant-module design. Import breadth in `studio.rs`,
  `crates/ms-tab-translation/src/tab.rs` and `layer_model/persist.rs` is legitimate: composition
  roots and a persistence boundary.

## Editing map
- Startup order, run modes, service flags, prompts, start routing, the post-window update:
  `startup/` (its `MODULE_README.md`) and `args.rs`.
- Main window, screen switching, close policy, loading / Closing screens, what can be opened,
  images delivered by other apps: `shell/` (its `MODULE_README.md`).
- Editor root wiring, per-frame order, hotkeys, panel dock, tab requests, page residency,
  teardown: `studio/` (its `MODULE_README.md`).
- Project save / page op / discard / unsaved cache: `studio/session/project/`. Image-set saving,
  its dialogs, hidden tabs and save hotkeys: `studio/session/image_set/`. What differs between
  the two kinds: `studio/session/`.
- Settings tab: `tabs/settings/`; the panes shared with the launcher (General, AI backend /
  ONNX selection, Tutorials, UI scale, section registry): crate `ms-settings-ui`.
- Version reporting and the git suffix: `crates/ms-config/src/version_format.rs` (pure rules) +
  `build.rs` (git probing, rerun watches); then the version contract above.
- `--check-venv` readiness: `crates/ms-installer/src/venv_check.rs` (decision) +
  `startup/service_flags.rs` (window, exit code) + `crates/ms-installer/src/utils.rs`
  (`required_dependency_specs`).
- Config defaults, runtime roots, model paths: `crates/ms-config/src/lib.rs`. Window placement
  and geometry: crate `ms-window-geometry` (runtime owner: the shell's `AppShell`). UI locale on
  disk: `ms_config::locale_store`. UI fonts: `ms_widgets::ui_fonts` + `fonts/ui/MODULE_README.md`.
- Python, GPU probes, the Hugging Face token, app-managed models: crate `ms-sysprobe`
  (`python_manager`, `gpu_utils`, `hf_token`, `ai_models`). Native ONNX runtime and the
  onnxruntime library: crates `ms-native-runtime`, `ms-onnx-runtime`; OCR / detection routing:
  `crates/ms-tab-translation/src/`.
- Chapter filesystem shape, load / save-merge, legacy migrations: crate `ms-project`.
  Structural page ops: crate `ms-page-ops`. Shared models: crate `ms-models`. Canvas: crate
  `ms-canvas`.
- Tabs: `crates/ms-tab-translation/src/`, `crates/ms-tab-cleaning/src/`,
  `crates/ms-tab-typing/src/`, `crates/ms-tab-ps-editor/src/`, `crates/ms-tab-page-manager/src/`,
  `crates/ms-tabs-simple/src/`. Launcher: crate `ms-launcher`. Installer / update: crate
  `ms-installer`. Reusable widgets: crate `ms-widgets`.
- Diagnostic programs: Cargo examples of the crate they exercise (`crates/<crate>/examples/`);
  this package has exactly one binary target (`main.rs`), the only one `build.rs` signs.
