# Module: crates/ms-os-integration/src

## Purpose
The one owner of the OS records that register a ManhwaStudio program copy with the operating
system: their names, the exact values they hold, and the operations that write, read and
delete them: the Windows half (registry entries, `.lnk` shortcuts, elevation) and the Linux half
(the per-user `.desktop` entry and its icon).

GUI-free and egui-free. Every operation is blocking and runs on the caller's thread; callers
(the installer, the binary's startup worker, the settings-warnings worker, the launcher's System
registration tab workers) own threading, progress and console reporting,
and presentation.

## Architecture
```text
ms-installer (utils.rs / install.rs)      sequencing per install kind, progress, messages
bin src/main.rs                           Linux entry check/write on a startup worker;
                                          routes the elevated helper (run_elevated_helper)
ms-settings-ui settings_warnings          probe() -> badges (badge_worthy + allowed_actions)
ms-launcher pages/system_registration.rs  probe() rows, allowed_actions buttons, apply() /
                                          apply_elevated() on its workers
        |   calls, maps IntegrationError::user_message()
        v
ms-os-integration
  identity.rs   names (product, publisher, exe, shortcut, registry subkeys)
  copy_identity.rs  CopyIdentity: the copy a record is written for (exe, program root)
  error.rs      IntegrationError (typed; user_message() = installer.utils.* texts)
  report.rs     registration report model, broken/stale split, badge rule, probe() dispatch
  actions.rs    action matrix, elevation rule, apply() (one refresh), helper tokens + result file
  windows/      values (pure tables) -> registry / shortcut / elevation (I/O); probe (judges + readers)
  linux/        desktop_entry (pure text, owner, decision) -> xdg (XDG dirs, writer, refresh);
                probe (judge + reader)
        |
        v
ms-config (input types), ms-log (diagnostics), ms-i18n (messages), windows-sys (Win32),
windows (COM: IShellLinkW / IPersistFile)
```

Layer position: at the `ms-sysprobe` level, below every consumer, so a consumer never needs
the eframe-based installer to name or touch a record. It must never depend on `ms-installer`,
`ms-settings-ui`, `ms-launcher`, egui or any crate above `ms-config`.

## Files and submodules
- `lib.rs`: crate root; module gates and the `IntegrationError` re-export.
- `identity.rs`: `PRODUCT_NAME`, `PUBLISHER`, `WINDOWS_EXE_NAME`, `SHORTCUT_FILE_NAME`,
  `UNINSTALL_SUBKEY`, `APP_PATHS_SUBKEY`, `APP_ICON_PNG` (embedded `app_icon_512.png`). Pure
  consts on every target.
- `error.rs`: `IntegrationError`, one variant per failure site, plus `Multiple` for
  independent operations that all ran.
- `windows/`: the Windows records; see `windows/MODULE_README.md`.
- `copy_identity.rs`: `CopyIdentity` (`exe`, `program_root`, `version_core`), the pure
  `program_root_for` rule, the repository-build rule `repo_build_root` (also
  `CopyIdentity::repo_build_root`) and `exe_program_root` (the root of a copy known only by its
  exe); `CopyIdentity::current()` = `current_exe()` + `ms_config::program_dir()`.
- `report.rs`: the registration report — `Scope`, `RecordKind`, `RecordValue`, `RecordReport`,
  `RecordStatus`, `Defect` + `DefectSeverity`, `ProbeError`, `RegistrationReport`; the rules
  `Defect::severity`, `badge_worthy`, `classify`; `copy_scope` and the blocking `probe()`
  (Windows and Linux only). Pure model, also compiled into host test builds.
- `actions.rs`: `ActionKind`, `ActionRequest`, the rules `allowed_actions` / `needs_confirmation`
  / `requires_elevation`, the in-process `apply()` (Windows and Linux), `ActionError` /
  `ActionFailure` / `ElevationError`, and the elevated helper's protocol: `encode_actions` /
  `decode_actions` (tokens), the result-file JSON (`helper_result_json`, `parse_helper_result`,
  `create_result_file`, `write_helper_result`, `read_helper_result`), `validate_result_path`,
  `new_result_path`, the helper's ordering `run_helper_protocol`, the
  `HELPER_EXIT_*` codes and the two flag spellings. Pure parts compiled into host test builds.
- `linux/`: the Linux desktop entry, its startup writer and its probe; see
  `linux/MODULE_README.md`.

## Contracts and invariants
- **One owner of names and values.** No other crate spells a record name (`ManhwaStudio.lnk`,
  `Uninstall\ManhwaStudio`, `App Paths\manhwastudio_rs.exe`, `Applications\manhwastudio_rs.exe`)
  or builds a record's value table; they import `identity` / `windows::values`.
- **Errors.** Operations return `IntegrationError`. Its `Display` is English for logs;
  `user_message()` is the user-facing text and renders `installer.utils.*` catalog entries (the
  pre-extraction texts for the old variants; the native registry variants add the system text
  of their Win32 code). The `error.rs` test pins every variant against the English catalog. A
  new variant needs a catalog key in `en.json` and `ru.json`.
- **Gating.** `error` and every dependency are native-only (`not(wasm32)`). The `windows` module
  is compiled on Windows AND into native host test builds, so pure tables and predicates are
  tested on Linux; every Win32, environment or process item inside is `target_os = "windows"`.
  The `linux` module mirrors it: compiled on Linux and into native host test builds, with
  `linux::xdg` (all I/O) gated to `target_os = "linux"`.
- **No GUI-thread work.** Registry calls are blocking Win32 calls, shortcuts are COM file I/O
  (each call enters and leaves its own apartment), the Linux writer does file I/O and awaits
  `update-desktop-database`: callers must run them on a worker.
- **Linux startup policy** (`linux::xdg::ensure_at_startup`, writes only under
  `$XDG_DATA_HOME`, never `$XDG_DATA_DIRS`): missing entry -> create; entry of this copy ->
  rewrite only when its bytes differ; entry of another copy -> never touched, even when that
  copy's executable is gone (repointing is an explicit user action); entry with no readable
  owner -> never touched. Ownership = `X-ManhwaStudio-Exe=` equal to `CopyIdentity::exe`, else
  (a legacy entry without the key) the first `Exec=` argument; a bare program name (no `/`) is
  resolved through `$PATH` first (`desktop_entry::resolved_entry_owner`, shared with the probe)
  and one found nowhere counts as no owner; paths compare canonicalized when both exist,
  lexically otherwise. `update-desktop-database` runs only after an entry write.
- **Program root** (`copy_identity::program_root_for`, the ONE owner of every working directory
  this crate writes: Linux `Path=`, the Windows `.lnk` working directory via
  `ShortcutSpec::for_copy` / `for_launcher`, the elevated helper's start directory): a
  repository build — the exe's directory lies directly in a directory named `target`
  (`<repo>/target/<any profile>/<exe>`; ASCII case-insensitive on Windows, exact elsewhere) — ->
  the directory containing `target`; else the executable directory when it holds the program
  markers; else the run's resolved runtime root when that does; else the executable directory.
  So `ms_config`'s cwd-first runtime-root resolution of a launch from a record lands on this
  copy. `exe` stays the built binary. Values that describe an INSTALL stay exe-based
  (`InstallLocation`, `DisplayIcon`, `UninstallString`, and App Paths `Path`, which Windows
  appends to `PATH` rather than using as a working directory). The Windows "Open with" command
  sets no working directory, so a repository build opened that way resolves its runtime root
  from the shell's working directory (known gap).
- **Registration probe** (`report::probe`): blocking, read-only, never panics; one record that
  cannot be read becomes `Unreadable` (logged) without affecting the others. Readers gather
  `Observed*` values (I/O); `evaluate_*` judges are pure over them, the `CopyIdentity` and an
  `exists` oracle (Linux adds the `is_ours` oracle = `xdg::same_executable`). Expected values
  come from the WRITERS (value tables, `ShortcutSpec`, `desktop_mime_types`), never a copy.
  A record's owner is the executable it launches; another copy's record is judged against that
  copy. A working directory still at the owner's exe directory where the program root moved
  off it (a repository build written before the rule) is `WorkingDirOutdated` (stale, Repair
  offered), any other existing-but-different one `WrongValue` (broken). `Defect::severity` is
  the only owner of the broken/stale split, `badge_worthy` the only owner of the badge rule (ours broken, another copy gone, another copy's record broken), and
  `classify` the only constructor of the owned / other statuses.
- **Actions** (`actions`): Missing -> `Create` (copy scope only); ours -> `Remove`, + `Repair`
  when defective; another copy's -> `RePoint` (copy scope or user scope only) + `Remove`, both
  confirmed while that copy exists; unreadable rows, Linux system-wide rows and `read_only`
  (`--ignore-installed`) -> none; the Linux "Open with" row never offers `Remove` (it shares the
  menu row's file). Create / Repair / RePoint write the full value set through the record writers
  (`windows::{registry, shortcut, values}`, `linux::xdg::write_entry`), never a copy of a table;
  Remove deletes whoever owns the record. `apply` runs every request independently and refreshes
  ONCE per batch (Windows `SHChangeNotify` after an App Paths / "Open with" action, Linux
  `update-desktop-database` after any entry write or delete). The Uninstall entry needs
  `CopyIdentity::version_core` (`MissingVersion` otherwise).
- **Elevated helper protocol.** Windows `Machine` actions of an unelevated process go through
  `windows::elevation::apply_elevated`: `ShellExecuteExW("runas")` of this exe with
  `--system-registration-apply <kind>:<scope>:<action>[,…]` and
  `--system-registration-result <temp_dir>/manhwastudio-sysreg-*.json`, wait on the process,
  exit code 0 (all ok) / 1 (some failed, incl. not elevated) -> read and delete the JSON
  `{"version":1,"results":[{"action","ok","error":{code,os,message,detail}}]}`; any other exit ->
  `HelperFailed`. UAC declined -> `Declined`. Only `Machine` tokens cross the boundary (encode
  and decode both reject `User`: an over-the-shoulder prompt runs the helper as another user).
  The helper never elevates (not elevated -> every action `NotElevated`, exit 1), writes only a
  NEW file with the fixed name pattern, which it CREATES BEFORE running any action
  (`run_helper_protocol`: exit 2 = nothing ran, no file; a write failure after the actions keeps
  exit 0/1, so the parent reports the result unreadable rather than "nothing happened"), and
  renders `message` in the UI locale of the shared user config. The parent deletes the result
  file on every error path after the launch (timeout, wait failure, other exit codes). A protocol change bumps `HELPER_RESULT_VERSION` and keeps
  both sides in this crate, so a launcher and its helper are always the same build.
- **Lints.** `clippy::all` only, no `clippy::pedantic`: the code was extracted verbatim from the
  installer (PROJECT_RULES "crates extracted ... do not enable pedantic"). A module written fresh
  here may opt in with a module-level `#![warn(clippy::pedantic)]`.

## Editing map
- To rename a record or change a product/publisher string, edit `identity.rs` (existing
  installs keep the old names on disk; plan a migration).
- To change what a registry record contains, edit `windows/values.rs` and its goldens.
- To change how registry keys are read, written or deleted, edit `windows/registry.rs`.
- To change shortcut location, target or writer, edit `windows/shortcut.rs`.
- To change the elevation probe or the UAC relaunch, edit `windows/elevation.rs`.
- To change the Linux entry's keys or MIME list, edit `linux/desktop_entry.rs` and its golden;
  to change the ownership rule or the startup decision, edit `entry_owner` / `decide_startup`
  there; to change where it is written or the XDG resolution, edit `linux/xdg.rs` (the startup
  call site is `src/main.rs::install_linux_desktop_integration_async`).
- To change what identifies a program copy or its program root, edit `copy_identity.rs`.
- To change which actions a record offers, how an action writes, or the helper protocol, edit
  `actions.rs` (+ its tables); the UAC launch and the helper entry point are in
  `windows/elevation.rs`; the CLI flags in `src/args.rs` must keep the spellings of
  `actions::SYSTEM_REGISTRATION_*_FLAG` (pinned by an args test).
- To change which defects badge or how statuses are built, edit `report.rs` (`Defect::severity`,
  `badge_worthy`, `classify`) and its tables; to change what a record is checked for, edit the
  judge in `windows/probe.rs` / `linux/probe.rs`.
- To add a failure, add an `IntegrationError` variant + catalog key + a case in the
  `error.rs` test.
