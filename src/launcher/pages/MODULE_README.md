# Module: src/launcher/pages

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
  validation through `ProjectValidationState`, open selection creation, and last-selection
  persistence in `user_config.json`.
- `import_page.rs`: `.mschapter` metadata read, editable target title/chapter form, archive
  extraction into the projects root, safe path validation, and optional open-after-import action.
- `export_page.rs`: title/chapter selection, project refresh, compression preset selection, and
  `tar + zstd` archive creation for `.mschapter` export.
- `settings_page.rs`: launcher settings tabs, projects-root persistence, system CPU/RAM/GPU probes,
  AI package probes, `General.ai_install_type` reconciliation, PyTorch/full-dependency upgrade
  flow, and a background-driven Python environment console.

## Contracts and invariants
- Page UI must stay responsive. Do not perform project scans, archive traversal, compression,
  Python probing, command execution, or installer work inside frame drawing.
- Page actions are routed by `LauncherApp`; pages must not launch the editor, updater, installer,
  or detached new-project window directly.
- Project root changes must be returned as `PageNavAction::ProjectsRootChanged` so `LauncherApp`
  can refresh every page and detached window that caches the root.
- `PageNavAction::OpenProject` must carry an `OpenProjectSelection` that has passed launcher-side
  validation.
- Import must reject unsafe archive paths and preserve explicit user-facing errors plus diagnostic
  log messages.
- Settings probes and consoles must use shared runtime helpers such as `python_manager` and
  `gpu_utils`; do not duplicate Python or GPU discovery in page UI code.

## Editing map
- To add a launcher-level navigation action, edit `base.rs`, update affected page states, and handle
  the action in `src/launcher/app.rs`.
- To change project opening, edit `open_page.rs`.
- To change archive import/export, edit `import_page.rs` or `export_page.rs`.
- To change global launcher settings, environment probes, AI install-type reconciliation, Torch
  upgrade UI, or the Python console, edit `settings_page.rs`.
