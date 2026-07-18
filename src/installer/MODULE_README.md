# Module: src/installer

## Purpose
Installer runtime used by the main startup path when the managed Python environment is missing or
when Windows service flags request installer maintenance actions.

## Architecture
`install.rs` owns the egui-facing installer state machines and windows. It gathers user choices,
starts background workers, and consumes progress events without blocking the GUI thread.
`update.rs` owns the Rust update window that is opened after the launcher returns an update intent,
when startup receives `--update`, or when hidden `--continue-update` resumes after executable
replacement.

`utils.rs` owns the non-UI installer/update backend: release lookup, downloads, executable
replacement handoff, archive extraction, managed Python/venv setup, static dependency
installation, optional full PyTorch setup, elevation helpers, Windows shortcuts/registry
integration, and uninstall cleanup.
Launcher settings reuse the same `utils.rs` PyTorch preflight/install helpers when upgrading a
base install to full or replacing the installed PyTorch wheel. Installer workers report progress
through typed events consumed by the UI; command output is surfaced as console/progress events
rather than blocking the frame loop.
For Windows installs under Program Files, the root install directory is created first and receives
an inheritable Users/Modify ACL before installer-managed files and subdirectories are created.

The first install no longer downloads application AI model weights. App-managed AI models are
resolved lazily by runtime code through `src/ai_models.rs`.

Installer dependencies are embedded in code as two groups: base dependencies installed for every
mode, and torch-dependent extras installed only after the full-mode PyTorch stage. The full extras
exclude `torch-directml`; PyTorch itself is installed by the explicit Torch stage.

## Files and submodules
- `mod.rs`: module boundary and public installer entry point export.
- `install.rs`: installer UI, event types, public startup/service functions.
- `update.rs`: update window shell, background release re-check, test-version override,
  executable-update progress, PyTorch choice continuation, completion state, and no-update exit.
- `utils.rs`: worker pipeline and platform helpers used by installer and updater UI.

## Contracts and invariants
- GUI windows must only poll channels and draw UI; downloads, filesystem work, command execution,
  and probes must run on background threads.
- Python discovery and command paths must go through `python_manager.rs`.
- GPU capability probes must go through `gpu_utils.rs`.
- Release asset lookup, uv download, app archive extraction, dependency installation, shortcuts,
  registry writes, and uninstall cleanup belong in `utils.rs`, not in egui window code.
- Direct HTTP file downloads must go through `utils.rs::download_asset` (retry with exponential
  backoff, HTTP Range resume into a `.part` file, size verification, atomic rename into place —
  the destination path never holds a partial file). GitHub API metadata requests must go through
  `utils.rs::github_api_get`, which retries transient failures and is shared with `update.rs`.
- Unsupported installer operations must return explicit user-facing errors.
- Initial installation must not eagerly download AI model weights.
- Fast installation must install only the base dependency group.
- Full installation must install PyTorch before torch-dependent extras.
- Successful installation records `General.ai_install_type` in the installed `user_config.json`:
  fast/base writes `Base`, full writes `Full`.
- The update window uses the installer's native window sizing. Release checks and updater work must
  run on background workers; the GUI thread only polls worker results, asks for PyTorch choice when
  needed, and draws state. Existing-install and custom-folder update entry points first query the
  target executable with `--version`, compare against GitHub releases, replace that executable, and
  launch the target copy with `--continue-update`.
- Update flow is two-stage: first replace the platform executable from the latest GitHub release,
  then resume with `--continue-update` to repair/create uv-managed `installer_files/venv`, refresh
  PyTorch only for Full installs when the embedded torch version is newer, install missing embedded
  dependency-list packages, and unpack `ManhwaStudio.zip` over the install root.
- Windows Program Files ACL changes happen at root directory creation time, not as a recursive
  post-install permission rewrite.
- Per-platform release binary assets are distinct: Windows `manhwastudio_rs.exe`, macOS
  `manhwastudio_rs_macos`, Linux bare `manhwastudio_rs`. `platform_binary_asset_name()` (present in
  both `update.rs` and `utils.rs`) selects among them via `cfg!(target_os = ...)`; both copies must
  stay in sync so the release check and the download stage agree on the asset.

## Editing map
- To change installer screens or user choices, edit `install.rs`.
- To change the update window shell, edit `update.rs`.
- To change install/update worker steps, release assets, command execution, or archive handling,
  edit `utils.rs`.
- To change PyTorch preflight, backend selection, or dependency groups, edit `utils.rs` and keep
  the UI choice types in `install.rs` synchronized.
- To change app-managed AI model download behavior, edit `src/ai_models.rs`, not this module.
