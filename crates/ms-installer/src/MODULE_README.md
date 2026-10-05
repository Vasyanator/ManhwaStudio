# Module: crates/ms-installer/src

## Purpose
Crate root of `ms-installer`, re-exported by the binary as `crate::installer`
(`src/main.rs`), so every `crate::installer::…` call site keeps working unchanged.
Installer runtime used by the main startup path when the managed Python environment is missing or
when Windows service flags request installer maintenance actions.

## Architecture
`install.rs` owns the egui-facing installer state machines and windows. It gathers user choices,
starts background workers, and consumes progress events without blocking the GUI thread.
`update.rs` owns the Rust update window that is opened after the launcher returns an update intent,
when startup receives `--update`, or when hidden `--continue-update` resumes after executable
replacement.

`install.rs` hosts two installer purposes (`InstallerPurpose`). `FullInstall` is the classic flow.
`EnvironmentRepair` (`run_environment_repair_window`, used by `--check-venv`) reuses the same window
and screens but fixes the target to the given root, starts on the dependency-profile screen, runs
`utils::run_environment_repair_worker`, and finishes with a plain "environment is ready" screen.

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
resolved lazily by runtime code through `ms_sysprobe::ai_models`.

Installer dependencies are embedded in code as two groups: base dependencies installed for every
mode, and torch-dependent extras installed only after the full-mode PyTorch stage. The full extras
exclude `torch-directml`; PyTorch itself is installed by the explicit Torch stage.

## Files and submodules
- `lib.rs`: crate boundary, public installer entry point export, and `HostVersion` — the
  application's own version pair, which the binary hands in at the entry points because a
  library cannot read the executable's `CARGO_PKG_VERSION` / `MS_APP_VERSION`.
- `install.rs`: installer UI, event types, public startup/service functions.
- `update.rs`: update window shell, background release re-check, test-version override,
  executable-update progress, PyTorch choice continuation, completion state, and no-update exit.
- `utils.rs`: worker pipeline and platform helpers used by installer and updater UI.
- `venv_check.rs`: the non-interactive, GUI-free readiness check behind `--check-venv`. It
  lives here rather than in `ms-sysprobe` because it reads `utils.rs`' own package
  requirements and shares `installed_torch_is_current` / `missing_specs_for_readiness` with
  the repair worker; `ms-sysprobe` sits BELOW this crate, so hosting it there would make
  the two circular.

## Contracts and invariants
- GUI windows must only poll channels and draw UI; downloads, filesystem work, command execution,
  and probes must run on background threads.
- Python discovery and command paths must go through `ms_sysprobe::python_manager`.
- GPU capability probes must go through `ms_sysprobe::gpu_utils`.
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
  fast/base writes `Base`, full writes `Full`. This is done by the UI on a successful worker
  result, for both installer purposes.
- The install target's `user_config.json` belongs to ANOTHER root than the running process's
  `data_dir()`, so it is always named as `ms_docstore::DocRef::new(root.join(USER_CONFIG_FILE),
  DocKind::UserConfig)` and never opened directly: the install-type record is one
  `ms_docstore::update` (backfills missing defaults, never overwrites a malformed file), and
  every other access (`venv_check`, the update worker's install-type probe) is a read-only
  `ms_docstore::read_value` that must not create or rewrite the document.
- Environment repair provisions ONLY the Python environment of an existing root. It must NEVER:
  download or extract `ManhwaStudio.zip`, copy the executable, write the app icon, create
  shortcuts, touch the Windows registry, run `finalize_windows_post_install`, request elevation,
  offer another install directory, or launch an installed copy. It must never delete an existing
  working environment either: an interpreter found by `python_manager` (including a user `venv/`
  in the root) is reused verbatim, and only a broken installer-owned `installer_files/venv` may be
  removed before a fresh uv-managed venv is created.
- Repair installs only the packages that `pip freeze` does not report (`missing_dependency_specs`),
  and skips the Torch stage exactly when `utils::installed_torch_is_current` says so.
- The required dependency set per install type is `utils::required_dependency_specs`: base group
  for `Base`/`None`, plus the torch extras for `Full`. PyTorch is NOT in that list: its requirement
  is a MINIMUM VERSION (`REQUIRED_TORCH_VERSION`), expressed by the single predicate
  `installed_torch_is_current`, which the readiness check (`src/venv_check.rs`), the repair worker
  and the update flow all share — a "ready" verdict must imply the worker has nothing left to do.
- Two views of "missing" exist on purpose. Installation uses the strict
  `missing_dependency_specs` (one concrete distribution per spec). Readiness uses
  `missing_specs_for_readiness`, which additionally accepts any interchangeable distribution of the
  same Python module (`ONNXRUNTIME_DISTRIBUTIONS`, mirroring `ms_sysprobe::ai_install_probe` plus the
  WebGPU build), because a working environment built on `onnxruntime-gpu`/`-webgpu`/`-migraphx`
  must not be offered a repair on every launch. Readiness may only ever be SOFTER than installation.
- `dependency_marker_matches_current_platform` understands exactly the two
  `platform_system == / != "Windows"` markers used by the embedded lists and treats anything else
  as "applies here" (fail-open: an unnecessary install, never a false readiness). A dependency
  with a different marker requires extending that function.
- Environment repair must leave a READY environment behind: if it cannot record
  `General.ai_install_type`, the outcome is `Failed`, not `Completed` (a silent failure there would
  make the next `--check-venv` reopen the installer forever). A full install keeps the older
  behavior, since startup re-detects the install type for a deployed app.
- The update window uses the installer's native window sizing. Release checks and updater work must
  run on background workers; the GUI thread only polls worker results, asks for PyTorch choice when
  needed, and draws state. Existing-install and custom-folder update entry points first query the
  target executable with `--version`, compare against GitHub releases, replace that executable, and
  launch the target copy with `--continue-update`. `--version` prints the EXTENDED version
  (`MS_APP_VERSION`, possibly `3.6.0+1cd9638-83-dirty`), so `run_update_binary_stage_inner` reduces
  both sides with `version_format::version_core` before `compare_version_strings`; the release
  comparators themselves are always fed the plain `CARGO_PKG_VERSION`. The extended string may be
  displayed (`UpdateApp::local_version_display`) but never compared.
- Update flow is two-stage: first replace the platform executable from the latest GitHub release,
  then resume with `--continue-update` to repair/create uv-managed `installer_files/venv`, refresh
  PyTorch only for Full installs when the embedded torch version is newer, install missing embedded
  dependency-list packages, and unpack `ManhwaStudio.zip` over the install root.
- Windows integration differs by install kind (`is_windows_all_users_install_dir`). All-users
  (Program Files, HKLM): App Paths, Uninstall entry, Start Menu shortcut and the "Open with"
  entry. Per-user (HKCU): ONLY the "Open with" entry. The "Open with" entry lives entirely under
  `{root}\Software\Classes\Applications\manhwastudio_rs.exe` (command `"<launcher>" "%1"`, the
  same exe the shortcuts target, `FriendlyAppName`, one `SupportedTypes\.<ext>` per
  `ms_config::single_image::input_extensions()`); it never creates a ProgID, `OpenWithProgids`
  or default handler, so no extension is taken over. Its values come from the pure, tested
  `windows_open_with_registry_values`; registration deletes the tree first. Registration is
  best-effort for both install kinds (`register_windows_open_with_best_effort`: logged and shown
  in the console, never fails an install). Uninstall deletes the tree only when its open command
  points into the uninstalled directory, and removes every registry key independently, reporting
  all failures together.
- Registry key existence is decided by Win32 status codes (`registry_key_presence`,
  `classify_registry_open_status`), never by `reg.exe` message text: that text is localized and
  OEM-code-page encoded, so a text match breaks on most Windows locales. Values read back for a
  decision go through `RegGetValueW` (UTF-16), not `reg query` output.
- Known debt: the "Open with" key `Applications\manhwastudio_rs.exe` is fixed per registry root.
  Two installs under one root share it: the last install wins, and uninstalling the owner removes
  it even if the other install remains (the other install's own uninstall then leaves it alone).
  A launcher with a fallback exe name (`resolve_windows_launcher_target`) still registers under
  the fixed key name.
- Windows Program Files ACL changes happen at root directory creation time, not as a recursive
  post-install permission rewrite.
- Release binary assets are per-platform × per-arch and distinct: Windows x86_64
  `manhwastudio_rs.exe` / aarch64 `manhwastudio_rs_arm64.exe`; macOS x86_64 `manhwastudio_rs_macos`
  / aarch64 `manhwastudio_rs_macos_arm64`; Linux x86_64 bare `manhwastudio_rs` / aarch64
  `manhwastudio_rs_linux_arm64`. A single `pub(crate) fn platform_binary_asset_name()` in `utils.rs`
  selects among them via `cfg!(target_os = ...)` × `cfg!(target_arch = "aarch64")` and is the one
  source used by both the release availability check (`update.rs`) and the download stage. On-disk
  executable names stay arch-agnostic (`platform_executable_file_name()`), so `_macos`/`_arm64`
  suffixes never leak into install-path or archive-strip logic.
- The Windows existing-install window (`install.rs`, `ExistingInstallApp`) offers seven choices and
  reports them as `ExistingInstallAction`. Two of them are deliberately narrow:
  - "replace the installed copy with this one" copies the RUNNING executable over the installed one
    and does NOTHING else: no `ManhwaStudio.zip`, no `installer_files/venv`, no registry or
    `DisplayVersion` refresh, no `--continue-update`. The installed Python payload therefore stays
    at its old version, which is safe only while the IPC `PROTOCOL_VERSION` still matches; the
    window states this in a warning that is always visible, not only on a version mismatch. The
    copy goes through `utils::replace_executable_with_local_file`: a staging file NEXT TO the
    target (so the final rename cannot cross a volume), then an atomic rename, with the staging
    file removed on every failure path — the target is never left missing or truncated. Write
    access is classified BEFORE anything is created (`classify_replace_target_access` over
    `has_write_access_for_install` + `is_running_elevated`). A Program Files install is normally
    writable WITHOUT elevation, because `prepare_install_root_dir` granted the built-in Users group
    inheritable Modify rights at install time, so a refusal is the exception and is worded as one:
    administrator rights are offered as one possible remedy, never as the headline. Elevation is
    out of scope — this action never requests UAC — and a rename refused because the destination is
    in use gets its own "the installed copy is running" message.
  - "run this copy standalone" only records `ExistingInstallAction::RunStandalone`; `main.rs`
    relaunches this executable with `--ignore-installed` prepended
    (`args::standalone_relaunch_args`) and exits. The flag cannot be turned on in process: startup
    routing consumes it and seeds the backend-socket `OnceLock` long before this window opens.
- Every background job of that window (the installed copy's `--version` probe, the reinstall, the
  replacement) reports through ONE `mpsc` channel owned by `ExistingInstallApp`; the GUI thread only
  drains it and draws. The probed version is the EXTENDED `MS_APP_VERSION` string: it is displayed
  as-is next to `env!("MS_APP_VERSION")` of the running copy and never compared. A failed probe is
  logged and shown as "unknown"; it never blocks any of the window's actions, because the user may
  be repairing exactly that broken install.
- Known limitation: external-target updates (existing-install / custom-folder entry points) pick the
  asset by the RUNNING process's os/arch and assume the target executable matches it; the target is
  only version-queried (`--version`), never arch-probed.

## Editing map
- To change installer screens or user choices, edit `install.rs`.
- To change the Windows "Open with" registration, edit `utils.rs::windows_open_with_registry_values`
  (data) and `finalize_windows_post_install` (which install kinds get it).
- To change what "replace the installed copy" does on disk, edit
  `utils.rs::replace_executable_with_local_file` and keep the exe-only contract above in sync.
- To change what environment repair does (or must not do), edit
  `utils.rs::run_environment_repair_worker` and keep the invariants above in sync.
- To change the update window shell, edit `update.rs`.
- To change install/update worker steps, release assets, command execution, or archive handling,
  edit `utils.rs`.
- To change PyTorch preflight, backend selection, or dependency groups, edit `utils.rs` and keep
  the UI choice types in `install.rs` synchronized.
- To change app-managed AI model download behavior, edit `ms_sysprobe::ai_models`, not this module.
