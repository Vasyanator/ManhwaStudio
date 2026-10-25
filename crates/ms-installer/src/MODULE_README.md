# Module: crates/ms-installer/src

## Purpose
Crate root of `ms-installer`, re-exported by the binary as `crate::installer`
(`src/lib.rs`), so every `crate::installer::…` call site keeps working unchanged.
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

`utils.rs` owns the non-UI installer/update backend: release lookup, the sequencing of
downloads (the network layer itself is `download.rs`), executable replacement handoff, archive extraction, managed Python/venv setup, static dependency
installation, the sequencing of the optional full PyTorch stage, the install-continuation elevation wrappers, the
sequencing of the Windows integration inside install and uninstall, and uninstall cleanup. The
Windows records themselves (registry keys, `.lnk` shortcuts, elevation probe and UAC relaunch,
their names and values) are owned by `ms-os-integration` (`crates/ms-os-integration/src/`);
`utils.rs` and `install.rs` only call them and turn `IntegrationError::user_message()` into
their messages.
`torch/` owns everything PyTorch-specific: the pin set and wheel options, the preflight, the
install command and stage, and the readiness predicate (see `torch/MODULE_README.md`).
Launcher settings reuse the same `torch::detect_torch_preflight` / `torch::run_torch_upgrade_worker`
when upgrading a base install to full or replacing the installed PyTorch wheel. Installer workers report progress
through typed events consumed by the UI; command output is surfaced as console/progress events
rather than blocking the frame loop.
For Windows installs under Program Files, the root install directory is created first and receives
an inheritable Users/Modify ACL before installer-managed files and subdirectories are created.

The first install no longer downloads application AI model weights. App-managed AI models are
resolved lazily by runtime code through `ms_sysprobe::ai_models`.

Installer dependencies are embedded in code as two groups: base dependencies installed for every
mode, and torch-dependent extras installed only after the full-mode PyTorch stage. PyTorch itself
is installed by the explicit Torch stage and appears in neither group. These two lists in
`utils.rs` are the ONE owner of the app's Python package set; the gitignored dev files
`requirements/base.txt` / `requirements/torch.txt` are hand-kept mirrors (same specs, same order;
comments allowed) that `utils::tests::requirements_files_mirror_installer_lists` compares when they
exist. `requirements/torch.txt` names no torch / torchvision, so a dev install from it never
replaces the PyTorch build the Torch stage chose.

## Files and submodules
- `lib.rs`: crate boundary, public installer entry point export, and `HostVersion` — the
  application's own version (`MS_APP_VERSION`, the commit count), which the binary hands in at
  the entry points because a library cannot read it.
- `install.rs`: installer UI, event types, public startup/service functions. Its folder picker
  (custom install location) is the non-blocking `ms_platform::file_dialog` (`PickDirectoryPath`,
  polled each frame); the finish screen's invite links open through `ms_platform::shell_open`.
- `update.rs`: update window shell, background release re-check, test-version override,
  executable-update progress, PyTorch choice continuation, completion state, and no-update exit.
- `utils.rs`: worker pipeline and platform helpers used by installer and updater UI: dependency
  lists, the pip / uv runner and its retry loop (`pub(crate)` for `torch/`).
- `torch/`: the PyTorch seam — pins (`catalog.rs`), choice types and the one choice widget
  (`choice_ui.rs`, drawn by the installer, the update window and the launcher), preflight, install command and
  stage, `run_torch_upgrade_worker`, the recorded choice store, the constraints file and the
  `torch_readiness` / `installed_torch_is_current` predicate.
- `release.rs` (+ `release/tests.rs`): the one owner of the app release feed
  (`APP_RELEASES_API`, `APP_ZIP_ASSET_NAME`, `APP_CHECKSUMS_ASSET_NAME`), of the selection of
  the release to update to / install from (`select_app_release`, pure; `fetch_latest_app_release`),
  of the continuation's own-version release (`select_app_release_by_tag`,
  `fetch_app_release_by_tag`) and of its `SHA256SUMS` manifest (`ChecksumsAsset::fetch`, strict
  `parse_sha256sums`, `ReleaseChecksums::pin_for`). `pub` because the binary's startup check selects with it too.
- `download.rs` (+ `download/tests.rs`): the network layer on `ms-download` — `GithubAsset`
  (name, URL, `size`, `digest`), `github_api_get` (`fetch_small`), `fetch_small_asset` (a small
  asset such as `SHA256SUMS`, exact listed size), `download_asset` (`fetch_verified`, identity
  sidecar, required sha256 pin, progress / retry text). Edit it for how anything is
  fetched; edit `utils.rs` for what is fetched and when.
- `prompt_window.rs`: the missing-environment startup prompt (install / update a custom install
  folder / open the launcher), returning `MissingPythonEnvAction`; the binary's
  `src/startup/prompts.rs` runs it and maps a failed window to `LaunchLauncher`. It uses
  `ms_window_geometry` for the shared window icon and the Android Activity handle.
- `window.rs`: `prepare_window_context(ctx)`, the one window-context preparation
  every window of this crate calls first in its creator closure.
- `pip_spec.rs`: the PEP 440 / PEP 508 subset (`Requirement`, `VersionSpecifier`,
  `Pep440Version`) that reads the embedded specs and decides whether an installed version
  satisfies a pin; unsupported forms (extras, URLs, `===`, wildcards) are typed errors.
- `venv_check.rs`: the non-interactive, GUI-free readiness check behind `--check-venv`. It
  lives here rather than in `ms-sysprobe` because it reads `utils.rs`' own package
  requirements and shares `torch::installed_torch_is_current` / `utils::missing_specs_for_readiness`
  with the repair worker; `ms-sysprobe` sits BELOW this crate, so hosting it there would make
  the two circular.

## Contracts and invariants
- Every `eframe::run_native` creator of this crate prepares its context ONLY through
  `window::prepare_window_context`: bundled UI fonts at `ui_fonts::Tier::Core` always, the studio
  theme (every window: installer, existing-install, uninstall, update, missing-environment
  prompt), then the process-global UI
  scale (`ms_widgets::ui_scale::apply_ui_scale_to_context`). The scale is whatever the slot holds
  when the window is created: the startup seed (`init_full`) runs before the update windows,
  the startup prompts and the `--check-venv` repair window; a launcher-requested update window
  sees the live value. The Windows `--uninstall` service window runs before that seed and
  therefore renders at the default 100 %.
- GUI windows must only poll channels and draw UI; downloads, filesystem work, command execution,
  and probes must run on background threads.
- Python discovery and command paths must go through `ms_sysprobe::python_manager`.
- GPU capability probes must go through `ms_sysprobe::gpu_utils`.
- Release asset lookup, uv download, app archive extraction, dependency installation, the calls
  that create shortcuts and registry entries, and uninstall cleanup belong in `utils.rs`, not in
  egui window code. Registry / shortcut / elevation primitives and record names belong in
  `ms_os_integration::windows`, never here; this crate must not depend on `windows-sys` for them.
- Direct HTTP file downloads must go through `download.rs::download_asset`, which runs
  `ms_download::fetch_verified` (`RetryPolicy::standard`, `Range` resume into `<file>.part`,
  15 s connect / 60 s read timeouts, fsync + rename; the destination never holds a partial or
  unverified file). The pin is the GitHub asset's `size` plus a sha256: the caller's
  `AssetDownload::required_sha256` when set, else the API's `digest` (`sha256:<hex>`) when
  published, else the size alone; a hash mismatch deletes the `.part`, is not retried and gets
  its own message (`installer.utils.download_checksum_mismatch_error`).
- **Every app download is verified against the release's `SHA256SUMS`** (`STATE.md` S-066):
  the platform binary of the update stage, `ManhwaStudio.zip` of the `--continue-update` stage
  and of a fresh install. The binary stage and the install select the newest release
  (`release::fetch_latest_app_release`; eligible only with the archive, this platform's binary
  AND `SHA256SUMS`). The `--continue-update` stage, running in the new binary, selects the
  release whose tag EQUALS its own `MS_APP_VERSION` (`release::fetch_app_release_by_tag`; archive
  + `SHA256SUMS` required), before it touches the environment, so a release published between
  the two stages can never mix an executable of one release with the files of another; a
  missing own release stops the stage with a user-visible error. The worker downloads the
  manifest (exact listed size; GitHub's sha256 digest of the manifest itself when listed),
  parses it — strict per-line syntax and no duplicate names; line order and completeness are
  NOT checked, pins are taken only for the assets this program downloads — takes the asset's entry with
  `ReleaseChecksums::pin_for` and passes it as `required_sha256`. Because `fetch_verified`
  checks the pin before publishing, the binary is verified before it can replace the program
  and the archive before it can be extracted. A malformed manifest, a missing entry, a GitHub
  digest that disagrees with the entry, or a downloaded file that does not match is a hard,
  logged, user-visible error; nothing is retried and the proven-wrong staged bytes are
  discarded. uv downloads keep the GitHub-digest pin (uv publishes no such manifest).
  Retries are shown as stage progress ("network error, retrying in N s (k/n)"), not as errors.
- Cross-run resume: `<file>.identity.json` (replaced atomically through `ms_atomic_file`) records the `{url, size, digest}` the staged files
  (`<file>.part`, the verified `<file>.download`) belong to; when it is missing, unreadable or
  names another asset, the staged files are deleted before the transfer, so bytes of another
  release are never resumed. A matching sidecar is necessary, not sufficient: `fetch_verified`
  resumes a `.part` of an earlier run only for a sha256-pinned asset (or a strong server ETag);
  a digest-less asset restarts from zero. The published destination is never deleted up front: it is
  replaced only by the final rename of a verified `<file>.download`, so a failed or offline
  attempt leaves a previously downloaded asset intact.
- GitHub API metadata requests must go through `download.rs::github_api_get` (`fetch_small`,
  16 MiB cap), shared with `update.rs`. `GITHUB_TOKEN` (when set) is sent as a bearer header on
  API and asset requests only through `ms_download::FetcherConfig`; it is never logged.
- Cancel: `InstallerApp` and `UpdateApp` own an `Arc<AtomicBool>` handed to every worker they
  start and set it in `Drop`, i.e. when the window closes (no window blocks a close during the
  install, so no separate Cancel button exists). Downloads, metadata requests and the retry
  backoff of `utils.rs::run_command_with_retry` stop on it; a running pip / uv child process is
  not interrupted. `run_torch_upgrade_worker` (launcher settings) has no cancel and passes a flag
  that never sets.
- `run_command_with_retry` retries every failed command (pip / uv exit codes cannot tell a
  network blip from an unsatisfiable requirement) up to the per-call-site attempt budget, waiting
  2, 4, 8 s (capped) between attempts.
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
- A version specifier in the embedded lists binds EXISTING environments too: `missing_dependency_specs`
  reports a spec whose package `pip freeze` does not list OR lists at a version the specifier
  rejects (PEP 440 order, local `+…` tags ignored), and installing that spec brings the package to
  the pin (upgrade or downgrade). An unparsable spec or installed version counts as missing
  (logged), never as satisfied. Every embedded spec must parse with `pip_spec::Requirement`
  (test `every_embedded_spec_parses`).
- Repair installs only the specs `missing_dependency_specs` reports,
  and skips the Torch stage exactly when `torch::installed_torch_is_current` (against the recorded
  torch choice, `torch::read_torch_choice_record`) says so.
- Every PyPI install that runs while a torch is installed — the torch extras after the Torch
  stage, repair (base and extras), the update continuation's missing dependencies and the
  launcher upgrade — passes `-c installer_files/torch-constraints.txt`
  (`torch::write_torch_constraints`, exact installed torch / torchvision, every flavour), so no
  later install can swap the chosen torch for the PyPI build. A fresh install's base step runs
  before any torch and has no constraints.
- The required dependency set per install type is `utils::required_dependency_specs` (used by
  readiness, repair and the update continuation alike): base group
  for `Base`/`None`, plus the torch extras for `Full`. PyTorch is NOT in that list: its requirement
  is a VERSION and FLAVOUR rule against the recorded choice (`torch::torch_readiness`), expressed
  by the single predicate `installed_torch_is_current`, which the readiness check
  (`src/venv_check.rs`), the repair worker and the update flow all share — a "ready" verdict must
  imply the worker has nothing left to do.
- Two views of "missing" exist on purpose. Installation uses the strict
  `missing_dependency_specs` (one concrete distribution per spec). Readiness uses
  `missing_specs_for_readiness`, which additionally accepts any interchangeable distribution of the
  same Python module (`ONNXRUNTIME_DISTRIBUTIONS`, mirroring `ms_sysprobe::ai_install_probe` plus the
  WebGPU build; `OPENCV_DISTRIBUTIONS` for `cv2`) — only for a spec without a version
  specifier — because a working environment built on `onnxruntime-gpu`/`-webgpu`/`-migraphx`
  must not be offered a repair on every launch. Readiness may only ever be SOFTER than installation.
- Before repair and the update continuation install into an existing environment, the colliding
  `cv2` distributions are uninstalled (`utils::conflicting_module_distributions`): when the
  required `opencv-python-headless` meets another `OPENCV_DISTRIBUTIONS` member, EVERY installed
  member is removed (shared files were overwritten) and the install pass reinstalls the required
  one. The onnxruntime family is never cleaned (a GPU build is a user choice).
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
  launch the target copy with `--continue-update`. `--version` prints the copy's
  `MS_APP_VERSION` — a commit count, or `3.6.0+1cd9638-83-dirty` for a copy built before
  commit-count versioning — so `run_update_binary_stage_inner` reduces both sides with
  `version_format::version_core` before `version_format::compare_release_versions` (the ONE
  release comparator, also used by the update window and the binary's startup check; never add
  a local copy). The update window compares and displays the running copy's
  `HostVersion::version` (`MS_APP_VERSION`). The releases API URL and the asset names are the
  `pub const`s in `release.rs`.
- Update flow is two-stage: first replace the platform executable from the latest GitHub release,
  then resume with `--continue-update` (archive from the release of the new binary's own
  version, see above) to repair/create uv-managed `installer_files/venv`, refresh
  PyTorch only for Full installs when the embedded torch version is newer, install missing embedded
  dependency-list packages, and unpack `ManhwaStudio.zip` over the install root.
- Which Windows records an install gets is decided HERE, by install kind
  (`ms_os_integration::windows::is_windows_all_users_install_dir`), in
  `utils.rs::finalize_windows_post_install`. All-users (Program Files, HKLM): App Paths,
  Uninstall entry, Start Menu shortcut and the "Open with" entry. Per-user (HKCU): ONLY the
  "Open with" entry (the finish screen may still add the per-user Start-menu and Desktop
  shortcuts the user ticked). "Open with" registration is best-effort for both kinds
  (`utils.rs::register_windows_open_with_best_effort`: logged and shown in the console, never
  fails an install). The Uninstall entry's `DisplayVersion` is the RUNNING program's
  version (`HostVersion::version` handed to `install::run_python_installer_window`, carried by
  `InstallerPurpose::FullInstall`). Uninstall (`run_windows_uninstall_worker`) removes
  shortcuts, then the registry entries that point into the uninstalled directory, then the
  files; when existing entries were left in place (another install's, or unverifiable) it shows
  a distinct "entries kept" status instead of "cleanup complete" (reasons in the session log). Install discovery (`install.rs::find_existing_windows_install`) reads `InstallLocation`
  / App Paths `Path` natively and treats an unreadable value as no candidate. What each record
  contains, the ownership rules on uninstall, Win32 status-code rules and the known debt live in
  `crates/ms-os-integration/src/windows/MODULE_README.md`.
- Every user-visible error of a Windows record operation is `IntegrationError::user_message()`,
  which renders the same `installer.utils.*` catalog texts; never show its `Display` (English,
  for logs) in the console or a dialog.
- Windows Program Files ACL changes happen at root directory creation time, not as a recursive
  post-install permission rewrite.
- Release binary assets are per-platform × per-arch and distinct: Windows x86_64
  `manhwastudio_rs.exe` / aarch64 `manhwastudio_rs_arm64.exe`; macOS x86_64 `manhwastudio_rs_macos`
  / aarch64 `manhwastudio_rs_macos_arm64`; Linux x86_64 bare `manhwastudio_rs` / aarch64
  `manhwastudio_rs_linux_arm64`. A single `pub(crate) fn platform_binary_asset_name()` in `utils.rs`
  selects among them via `cfg!(target_os = ...)` × `cfg!(target_arch = "aarch64")` and is the one
  source used by the release selection (`release::select_app_release`), hence by every check and
  download. Releases no longer publish the Windows / Linux aarch64 binaries, so builds for those
  targets compile but find no updatable release. On-disk
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
  - "run this copy standalone" only records `ExistingInstallAction::RunStandalone`; `lib.rs`
    relaunches this executable with `--ignore-installed` prepended
    (`args::standalone_relaunch_args`) and exits. The flag cannot be turned on in process: startup
    routing consumes it and seeds the backend-socket `OnceLock` long before this window opens.
- Every background job of that window (the installed copy's `--version` probe, the reinstall, the
  replacement, the Desktop / Start-menu shortcut buttons on the `existing-install-shortcut`
  worker) reports through ONE `mpsc` channel owned by `ExistingInstallApp`; the GUI thread only
  drains it and draws. The probed version is the installed copy's `MS_APP_VERSION` string: it is
  displayed as-is next to the running copy's `HostVersion::version` and never compared. A failed probe is
  logged and shown as "unknown"; it never blocks any of the window's actions, because the user may
  be repairing exactly that broken install.
- The finish screen of a full install writes the ticked Desktop / per-user Start-menu shortcuts
  on the `install-finish-shortcuts` worker (`InstallerApp::finish_with_shortcuts`), never on the
  GUI thread: the screen switches to `UiState::CreatingShortcuts`, and the outcome the user chose
  ("open" / "close") is applied — window closed — only after the worker's report arrived. While
  it runs, an OS close request is cancelled, so the process never exits under a half-written
  `.lnk`. A failed shortcut is a console line, never a failed install.
- Known limitation: external-target updates (existing-install / custom-folder entry points) pick the
  asset by the RUNNING process's os/arch and assume the target executable matches it; the target is
  only version-queried (`--version`), never arch-probed.

## Editing map
- To change installer screens or user choices, edit `install.rs`.
- To change what a Windows record contains (registry values, shortcut target, key names), edit
  `ms-os-integration` (`windows/values.rs`, `windows/shortcut.rs`, `identity.rs`); to change
  which install kinds get which record, edit `utils.rs::finalize_windows_post_install`.
- To change what "replace the installed copy" does on disk, edit
  `utils.rs::replace_executable_with_local_file` and keep the exe-only contract above in sync.
- To change what environment repair does (or must not do), edit
  `utils.rs::run_environment_repair_worker` and keep the invariants above in sync.
- To change the update window shell, edit `update.rs`.
- To change how release metadata or assets are fetched (retry, resume, pins, progress text),
  edit `download.rs`.
- To change which release is chosen, the release asset names, or the `SHA256SUMS` format and
  pin rules, edit `release.rs` (and keep the release script's manifest in step).
- To change install/update worker steps, release assets, command execution, or archive handling,
  edit `utils.rs`.
- To change PyTorch pins, preflight, wheel options, the install command or the readiness rule,
  edit `torch/`; to change dependency groups or a package pin, edit `BASE_DEPENDENCIES` /
  `TORCH_DEPENDENCIES` in `utils.rs`, then mirror it into `requirements/*.txt`.
- To support another specifier or version form, edit `pip_spec.rs`.
- To change app-managed AI model download behavior, edit `ms_sysprobe::ai_models`, not this module.
