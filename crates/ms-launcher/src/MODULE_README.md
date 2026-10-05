# Module: crates/ms-launcher/src

## Purpose
Rust launcher runtime shown before a chapter is opened.

## Architecture
`lib.rs` (crate root) owns the native launcher entry points (`run_launcher`, `run_test_launcher`)
and returns a typed outcome to `main.rs`.
`app.rs` owns the root `eframe::App`, background image workers, detached child windows, and
page navigation. Page modules render focused launcher workflows and report actions through
`PageNavAction`.

The launcher does not perform blocking I/O on the GUI thread. Startup update checks are started by
`main.rs` and delivered to the launcher through a channel; the launcher only polls and renders the
notification.

## Files and submodules
- `lib.rs`: crate root; launcher window setup, app metadata, and public run functions.
- `app.rs`: root app state, worker polling, page routing, detached viewport handling; starts
  and polls the settings-warnings checks (see "Settings warnings").
- `main_page.rs`: central menu (the Settings button carries the overall settings-warning
  corner badge), update notification overlay, AI install-type notices, and the
  storage-mode conversion status line (progress of the process-wide `storage_mode_job`, then a
  dismissable failure notice). Also the native-only small "Open image" button (drawn in a
  `Ui::new_child` over the title -> grid gap, so it takes no layout space) and its picking status.
- `open_image.rs` (native only): single-image mode entry — `rfd` file picker plus path
  validation (exists + is a file) on a worker, the filter built from
  `ms_config::single_image::input_extensions()` (plus uppercase copies for case-sensitive GTK
  globs), and the pure pick -> `LauncherOutcome::OpenImage` mapping. `app.rs` polls it.
- `state.rs`: page enum, shared UI state, and typed launcher outcomes.
- `background.rs`: background image plan and decode workers.
- `first_run_language.rs`: first-run interface/typesetting language-selection modal
  (radio toggles, system-locale preselect). Reuses `general_settings_panel`'s
  `pub(crate)` scan/install/persist helpers; blocks input with the tutorial-engine
  overlay pattern. Edit here to change the modal's detection, preselection, or layout.
- `pages/`: fullscreen launcher pages for open/import/export/settings flows.
- `new_project/`: detached new-project workflow. Its ribbon and crop-editor previews are drawn
  only through `new_project/ribbon.rs` over the `egui-large-image` crate (tiled, budgeted uploads;
  see `new_project/MODULE_README.md`).
- `psd_import_window.rs`: detached PSD/PSB import workflow. Both formats take the same path:
  `ag-psd` tells them apart by the version field in the file header, and the accepted
  extensions live in one place (`is_supported_document_ext`). Plain raster files
  (`is_supported_image_ext`: png/jpg/jpeg/bmp/webp/tif/tiff) found in the same selection,
  folder or archive are imported too — each becomes a one-row single-page document whose
  image is the source page, so it reuses the flattened-PSD `LayerSource::Composite` path.
  `load_source_from_bytes` is the per-file reader switch; all four scanners go through it.
  Layer rows carry a fourth action beside skip/source/clean: "overlay on clean"
  (`LayerImportType::OverlayOnClean`) writes no file of its own and is instead composited
  source-over onto the page's clean image at the layer's own PSD canvas offset, clipped to
  it, before that image is saved. Several overlays may share a page; they are applied in PSD
  hierarchy order and require a clean row on the same page (`validate_all_rows`). The saved
  clean is named by the binding owner (`ms_page_ops::clean_binding::clean_overlay_file_name` of
  the source page's stem), never by restating `<stem>.png`. The action
  is offered only for a `LayerSource::Layer` row of a document with three or more layer rows.
  Note the layer order this depends on: `ag-psd` preserves the raw PSD record order, so
  `LoadedPsdDocument::layers` and the rows built from it are BOTTOM-FIRST (index 0 =
  bottommost). The preview of an overlay row is composited on the preview worker thread and
  cached under `"{row_key}@on:{clean_row_key}"` so reassigning the clean row rebuilds it.
  Every preview is painted over a repeating transparency checkerboard (one cached texture, one
  quad with a repeating UV rect), not over the flat dark container fill: transparent holes must
  read as holes rather than as black ink.
  Two deliberate non-goals of the PSB work, not gaps to be "fixed" on sight: (1) there is no PSB
  EXPORT — the typing tab's `TypingExportFormat` (`crates/ms-tab-typing/src/tab.rs`) offers `Png`/`Psd` only,
  and `psb` exists nowhere outside this module; (2) the font-card import filter stays `psd`-only
  (`src/tabs/settings/typesetting/font_groups.rs`, the rfd `add_filter` for the font card), even
  though `ag-psd` would read a `.psb` there. Both are scope decisions; widen them only on request.
- `theme.rs`: launcher visual style helpers, including `notice_banner` (the amber one-action
  notice used by the open page's recovery and chapter-format banners).
- `tutorial.rs`: step script for the main-menu tour (`TutorialId::LauncherMain`); its target keys
  match the `mark` calls in `main_page.rs`.

## Tutorial wiring
`app.rs` owns a `TutorialController<LauncherState>` (lighter dim so the wallpaper stays visible) and
shares its `TutorialProgressHandle` with `settings_page` (the "Обучение" tab). Per-frame in `fn ui`:
edge-triggered `maybe_autoplay(LauncherMain)` on entering the main page → `sync` + `begin_frame`
before the panel → `main_page.rs` records button rects via `app.tutorial.mark(...)` → `render` after
the child windows. See `crates/ms-settings-ui/src/tutorial/MODULE_README.md` for the engine contract.

## Settings warnings
Per-setting "!" badges (core: `ms_settings_ui::settings_warnings`, see its MODULE_README).
- **Owner:** `SettingsPageState::warnings` (one `SettingsWarnings` per launcher entry). The main
  page reads it only through `LauncherApp::settings_warning_level`; nothing copies the set.
- **Lifecycle:** a full check run on EVERY launcher entry (program start and return from the
  studio, since `LauncherApp` is rebuilt per entry). `LauncherApp::new` has no egui context and
  also serves `web_entry.rs`, so `poll_workers` starts the run on the first frame
  (`ensure_warning_checks_started`, idempotent) and polls it every frame (`poll_warnings`,
  repaint on change). `--no-ai` reaches the checks as `CheckContext::ai_enabled == false`
  (from the backend handle). On wasm the runtime is inert: no badges.
- **Recheck routing:** only the evaluation units a `SettingChange` affects are re-run.
  Sources: the shared panes' `changed_settings` (forwarded by the settings page after each
  shared draw); the AI pane's landed off-thread writes, drained EVERY frame through
  `SharedSettingsPanels::take_landed_changes` whatever page or tab is shown; the first-run
  language modal confirm (`UiLanguage`, in `app.rs`); a reconciled install type
  (`AiInstallType`, inside `SettingsPageState::set_ai_install_type`, reached via
  `PageNavAction::AiInstallTypeChanged`); the projects folder (`ProjectsRoot`) on the falling
  edge of `LauncherState::project_creator_open` (Import page, new-project and PSD-import
  windows), since a create or import can create the folder without any config write — the
  one place for every create path, in `LauncherApp::poll_workers`. A new launcher-side writer
  of a checked setting (or a new surface that creates the projects folder) must be added here.
- **Badges** are paint-only (`paint_corner_badge`): the menu button and the tab buttons keep
  their click and tutorial-target rects.

## Contracts and invariants
- Launcher outcomes are returned to startup flow; the launcher must not spawn a second main app.
- Long scans, image decoding, probes, downloads, and shell work run on worker threads.
- Settings changes to the projects root must be propagated to every page/window that caches it.
- "Open image" closes the launcher with `LauncherOutcome::OpenImage(path)`; `main.rs` routes it.
  The launcher validates only existence and file-ness; decode errors belong to the studio
  loading screen (one owner). wasm has no button and `web_entry.rs` ignores the variant.
- Update notifications are advisory UI state; starting an update closes the launcher and returns
  `LauncherOutcome::StartUpdate` to `main.rs`.
- The first-run language modal (`first_run_language.rs`) and the main-menu tutorial are mutually
  exclusive: while the modal is `Some`, `app.rs` suppresses `maybe_autoplay(LauncherMain)` and hands
  the tour off exactly once on confirm. The modal is gated on the persisted tri-state marker
  `General.first_run_languages_confirmed` (present + `false` = show), NOT on the language keys being
  absent — the startup flow persists the defaults tree (materializing both language keys) before the
  launcher constructs, so `config::mark_first_run_languages_if_needed` records the marker earlier, before
  any defaults-persisting call. The modal persists nothing until confirmed (then writes both languages
  plus the marker `= true`); a config read error skips it (never blocks the user).

## Editing map
- **Reproducing a first run** (the language modal, or anything else gated on a virgin config):
  launch the built binary with the working directory set to an EMPTY scratch dir and pass
  `--test-launcher --no-ai`. `config::resolve_runtime_root()` finds no program markers in either
  the cwd or the exe dir and falls back to the cwd, so the whole config tree — including
  `General.first_run_languages_confirmed` — is created inside that scratch dir and the real
  installation is untouched.
- To change launcher startup or return values, edit `lib.rs` and `state.rs`.
- To change root polling, page routing, or window lifecycle, edit `app.rs`.
- To change the main menu or update notice, edit `main_page.rs`.
- To change the "Open image" picker, its filter or validation, edit `open_image.rs`; the accepted
  types themselves live in `ms_config::single_image::INPUT_FILE_TYPES`.
- To change a specific page workflow, edit `pages/`.
- To change when settings warnings run or which launcher events recheck them, edit
  `app.rs` (`poll_workers`, first-run confirm) and `pages/settings_page.rs` (`*_warning*`); badge
  placement: `main_page.rs` / `show_tab_button_impl`; the checks themselves live in
  `ms_settings_ui::settings_warnings`.
