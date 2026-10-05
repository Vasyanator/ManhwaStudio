# Module: crates/ms-launcher/src/pages

## Purpose
Fullscreen page workflows used inside the Rust launcher shell before a project is opened. These
pages cover opening existing chapters, importing/exporting `.mschapter` archives, and editing
launcher-wide settings.

## Architecture
`LauncherApp` owns page instances, routing, transitions, detached window lifecycle, and final
launcher outcomes. Page modules render focused workflows and return `PageNavAction` values through
`base.rs`; they do not start the editor, updater, or installer directly.

Each state type follows the same pattern: store input/status fields, start a worker by keeping an
`mpsc::Receiver`, poll that receiver from `show`, and return a typed navigation action when the
root launcher needs to react. Filesystem scans, archive work, project validation, Python probes,
installer preflights, shell I/O, and system probes run on workers and request repaint when new
state arrives.

## Files and submodules
- `mod.rs`: module declarations for the launcher page stack.
- `base.rs`: shared slide/fade transition runtime, clipped page layer creation, common page shell,
  back button, and `PageNavAction`.
- `open_page.rs`: projects-root title/chapter scanning, `_unsaved` chapter detection, project
  validation through `ProjectValidationState`, open selection creation, last opened title
  persistence, and per-title last opened chapter persistence in `user_config.json`. Also the
  chapter storage-format banner: the validation worker adds
  `project_scan::chapter_storage_report` (committed + `_unsaved` owned documents), and a
  chapter not in `default_format()` gets a Convert offer run on the `launcher-chapter-convert`
  worker (`project_scan::convert_chapter_storage`). The same worker parses the title's
  unsaved session (`project_scan::damaged_unsaved_documents`); a damaged session disables
  Restore, names the damaged files, and offers only the discard path ("Discard session and
  open" = plain Open, which deletes `{chapter}_unsaved`). Committed-tree damage stays a
  non-discardable warning. Nothing is deleted without that explicit click.
- `import_page.rs`: `.mschapter` metadata read, editable target title/chapter form, archive
  extraction into the projects root, safe path validation, and optional open-after-import action.
- `export_page.rs`: title/chapter selection, project refresh, compression preset selection, and
  `tar + zstd` archive creation for `.mschapter` export.
- `settings_page.rs`: launcher settings tabs, system CPU/RAM/GPU probes,
  AI package probes, `General.ai_install_type` reconciliation, PyTorch/full-dependency upgrade
  flow, and a background-driven Python environment console. The tab set, ordering, tab labels, and
  the shared General/AiBackend/Tutorials sections come from the shared section registry
  (`crate::settings_shared`): `active_tab` is a `SettingsSectionId`, the tab bar iterates
  `sections_for(SettingsSurface::Launcher)`, and the three shared "double-interface" panels are owned
  as one `SharedSettingsPanels`. The dynamic TorchUpgrade hide/relabel logic is applied inline in the
  tab bar. The launcher-exclusive sections (SystemInfo/AiComputations/TorchUpgrade/PythonEnvironment)
  keep their local renderers here. The `ProjectsRootChanged` invariant is unchanged — a saved
  projects root is still emitted as `PageNavAction::ProjectsRootChanged` (mapped from the shared
  General section's outcome). The page also OWNS the launcher's `SettingsWarnings` (started on
  the entry's first frame by `LauncherApp::poll_workers`): it passes the set to the shared
  panes (item badges), paints each tab's worst level as a corner badge in
  `show_tab_button_impl` (`highlighted` there is the unrelated amber upgrade-tab look), and
  rechecks the units named by every `SettingChange` the panes report.

## Contracts and invariants
- Page UI must stay responsive. Do not perform project scans, archive traversal, compression,
  Python probing, command execution, or installer work inside frame drawing.
- Page actions are routed by `LauncherApp`; pages must not launch the editor, updater, installer,
  or detached new-project window directly.
- Project root changes must be returned as `PageNavAction::ProjectsRootChanged` so `LauncherApp`
  can refresh every page and detached window that caches the root.
- A storage-mode switch started from the settings page is reported, once its conversion job
  finished, as `PageNavAction::StorageModeChanged` (mapped from the shared General outcome);
  `LauncherApp::apply_storage_mode` is the refresh hook for format-dependent page state.
  It calls `OpenPageState::revalidate_selection` so the chapter-format banner is re-probed.
- Chapter storage format: a chapter keeps its format across a mode switch and stays openable
  when it mismatches; converting it is only an explicit user action. Open/Restore are disabled
  while the chapter conversion runs, Convert is disabled while the process-wide
  `storage_mode_job` runs (that job moves `default_format()`, the conversion target), and an
  unreadable document replaces the Convert offer with a warning. The chapter worker holds a
  `storage_mode_job::ChapterConversionLease` (taken before the target is read, released before
  the result is sent), so the global switch is refused and its radio disabled meanwhile.
- Settings warnings: every write of a checked setting must reach
  `SettingsPageState::recheck_warnings` (the shared panes report theirs through
  `changed_settings` / `take_landed_changes`; a launcher-side writer calls it directly). The
  checks themselves run only on the `settings-warnings` worker, never in `show`.
- The settings page delivers every action a frame produced (e.g. a saved projects root AND a
  finished storage switch): extras are queued and returned one per frame, in order.
- Notice banners (one text + at most one action) use `theme::notice_banner` with a stable
  `id_salt`; do not hand-roll another `Frame`.
- `PageNavAction::OpenProject` must carry an `OpenProjectSelection` that has passed launcher-side
  validation.
- Import must reject unsafe archive paths and preserve explicit user-facing errors plus diagnostic
  log messages.
- Settings probes and consoles must use shared runtime helpers such as `python_manager` and
  `gpu_utils`; do not duplicate Python or GPU discovery in page UI code.

## Editing map
- To add a launcher-level navigation action, edit `base.rs`, update affected page states, and handle
  the action in `crates/ms-launcher/src/app.rs`.
- To change project opening, edit `open_page.rs`.
- To change archive import/export, edit `import_page.rs` or `export_page.rs`.
- To change global launcher settings, environment probes, AI install-type reconciliation, Torch
  upgrade UI, or the Python console, edit `settings_page.rs`.
