# Module: crates/ms-os-integration/src/windows

## Purpose
The Windows records of a ManhwaStudio install: the Uninstall and App Paths registry entries,
the "Open with" registration, the `ManhwaStudio.lnk` shortcuts, plus the elevation probe and the
UAC relaunch that writing all-users records needs. Which install kind gets which record is
decided by the caller (`ms-installer` `utils.rs::finalize_windows_post_install`).

## Architecture
- `mod.rs`: install-kind rule (`is_windows_all_users_install_dir` = under `%ProgramFiles%` /
  `%ProgramFiles(x86)%`), `windows_uninstall_registry_root` (`HKLM` vs `HKCU`),
  `normalize_windows_path`, and the private `to_wide` helper.
- `values.rs` (pure): `windows_open_with_registry_values`, `uninstall_and_app_paths_values`,
  key builders (`windows_open_with_app_key`, `uninstall_key`, `app_paths_key`), the Win32
  payload encoding (`RegistryValue::encode`, `encode_reg_sz`), `quote_windows_arg`, and the
  ownership predicates (`open_with_command_targets_install`, `uninstall_entry_ownership`,
  `app_paths_entry_ownership` -> `RecordOwnership`).
- `registry.rs`: native Win32 registry I/O (`RegOpenKeyExW`, `RegCreateKeyExW`,
  `RegSetValueExW`, `RegDeleteTreeW` + `RegDeleteKeyExW`, `RegGetValueW`), the
  `SHChangeNotify` association refresh, and the record writers / removers built on `values.rs`.
- `shortcut.rs`: the pure `ShortcutSpec` (`for_launcher`), the native `write_shortcut` /
  `read_shortcut` (`IShellLinkW` + `IPersistFile` over the `windows` crate, `ShortcutTarget`),
  shortcut folders from the shell's known folders, launcher target resolution, removal.
- `probe.rs`: the registration probe — readers (`target_os = "windows"`) of the Start-menu
  `.lnk` (`FOLDERID_Programs` / `FOLDERID_CommonPrograms`, `read_shortcut`), Uninstall, App Paths
  and "Open with" keys (`reg_read_string`) under BOTH roots, and the pure judges
  `evaluate_start_menu` / `evaluate_program_entry` / `evaluate_app_paths` / `evaluate_open_with`.
- `elevation.rs`: `is_running_elevated` (token elevation), the fire-and-forget
  `relaunch_self_elevated_with_args` (`ShellExecuteW("runas")`), and the system-registration
  helper round trip: `apply_elevated` (`ShellExecuteExW("runas")` +
  `SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC`, wait in <= 500 ms slices up to a timeout,
  `GetExitCodeProcess`, result file) and `run_elevated_helper` (the elevated side; protocol in
  `../actions.rs`).

## Contracts and invariants
- **Records.** App Paths `{root}\…\App Paths\manhwastudio_rs.exe`: default = launcher, `Path` =
  install dir (the exe's directory, also for a repository build: Windows appends it to `PATH`,
  it is not a working directory). Uninstall `{root}\…\Uninstall\ManhwaStudio`: `DisplayName`, `Publisher`,
  `DisplayVersion` (the installed program's `version_core`), `InstallLocation`, `DisplayIcon` = launcher, `UninstallString` = `QuietUninstallString` =
  `"<launcher>" --uninstall`, `NoModify` = `NoRepair` = 1 (DWORD). "Open with" lives entirely
  under `{root}\Software\Classes\Applications\manhwastudio_rs.exe`: `FriendlyAppName`, command
  `"<launcher>" "%1"` (the same exe the shortcuts target), one empty `SupportedTypes\.<ext>`
  per `ms_config::single_image::input_extensions()`. Never a ProgID, `OpenWithProgids` or
  default handler: no extension is taken over. Registration deletes the tree first.
- **Value order is part of the contract**: writers stop at the first failure, so the goldens in
  `values.rs` pin order as well as content.
- **Ownership on uninstall.** Nothing is deleted without proof that it belongs to the
  uninstalled directory. "Open with": its command launches an exe directly inside it
  (`open_with_command_targets_install`). Uninstall entry: `InstallLocation` is the directory and
  the `UninstallString` exe lies directly in it. App Paths: the default value (launcher) lies
  directly in it and `Path` is it. Every PRESENT, non-blank identifying value must agree
  (`RecordOwnership::Owned`); another copy's entry (`Foreign`), an entry without identifying
  values (`NoEvidence`), or one whose existence or values cannot be read is left in place and
  logged. `remove_windows_registry_entries_for_install` attempts every key independently and
  returns all failures as `IntegrationError::Multiple`; on success its
  `RegistryRemovalSummary::kept` lists every existing entry it left in place with a
  `KeptReason` (`OtherInstall`, `NoEvidence`, `Unreadable(code)`), so a caller never reports
  "complete" over a kept entry.
- **Native registry, 64-bit view, Win32 status codes.** No `reg.exe`: every key is opened or
  created with `KEY_WOW64_64KEY`, every read uses `RegGetValueW` + `RRF_SUBKEY_WOW6464KEY`
  (UTF-16, `REG_SZ` only; a `REG_EXPAND_SZ` comes back expanded; a data call that reports
  `ERROR_MORE_DATA` is retried, bounded, with the buffer regrown to the size it reported). Existence and failures are
  numeric Win32 codes (`classify_registry_open_status`), carried in `IntegrationError`'s
  `Registry*` variants. A tree delete opens the key itself in the 64-bit view, empties it with
  `RegDeleteTreeW` and removes it with `RegDeleteKeyExW` (`RegDeleteTreeW` has no view flag);
  the open requests exactly `DELETE | KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE | KEY_SET_VALUE`.
- **Explorer refresh.** Every public operation that writes or deletes an "Open with" or App
  Paths key ends with ONE `SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST)` (also after a
  partial failure). An all-users install runs two such operations (registry entries, then
  "Open with"), so it notifies twice. The one exception is `write_windows_open_with`, the
  non-notifying building block of `register_windows_open_with`: its caller must call the public
  `notify_shell_association_change` itself, also after a failure.
- **Shortcuts.** A launcher shortcut targets the launcher exe, takes icon `<exe>,0` and starts in
  the copy's program root: `ShortcutSpec::for_copy` (the identity's `program_root`, used by the
  registration actions and to judge this copy's link) or `ShortcutSpec::for_launcher` (derived
  from the exe alone: the repository root of a `<repo>/target/<profile>/` build, else the exe's
  directory — the installer's own shortcuts and another copy's link). Folders are the shell's known
  folders: `FOLDERID_Desktop` (must exist; a OneDrive-redirected Desktop is followed),
  `FOLDERID_Programs` for a per-user and `FOLDERID_CommonPrograms` for an all-users install
  (created on write). A folder the shell cannot resolve is logged and reported as
  `DesktopNotFound` / `StartMenuFolderNotFound`. Removal is by file name and visits both the
  known folder and the `%USERPROFILE%` / `%APPDATA%` / `%ProgramData%` path earlier builds wrote
  to, each distinct directory once.
- **COM.** Every `write_shortcut` / `read_shortcut` call enters an apartment-threaded COM
  apartment on the calling thread and leaves it on return (`ComApartment`); on a thread that
  already runs a multithreaded apartment (`RPC_E_CHANGED_MODE`) it uses that one and does not
  uninitialize it. Failures carry the failing step and the `HRESULT`
  (`IntegrationError::ShortcutWrite` / `ShortcutRead`).
- **Probe.** Both roots are read for every record. Owner = the executable a record launches
  (Uninstall: quoted `UninstallString` exe, else `DisplayIcon`, else `InstallLocation` +
  exe name, else the unquoted command's first token; App Paths: default value, else `Path` +
  exe name; "Open with": the command's exe; `.lnk`: its target); no owner -> `Unreadable(NoOwner)`.
  `probe::resolve_owner` decides over those candidates: ours when ANY of them equals
  `CopyIdentity::exe` by the path rule of `values::normalize_windows_path_text`; else the first
  one named `manhwastudio_rs.exe` (ASCII case-insensitive) is another copy; else the record
  launches a foreign program (`OursBroken` with `ForeignProgram`, nothing else judged). This
  probe rule is separate from the uninstall deletion rule above, which still needs EVERY value
  to agree.
  Expected values are the writer tables evaluated for the OWNER; paths compare by the path rule,
  display texts exactly, commands as `"<exe>" <tail>` with the tail exact (an unquoted or
  differently-tailed command is `MalformedCommand`). `DisplayVersion` is judged only for this
  copy's entry when the identity carries a version; `NoModify` / `NoRepair` (`REG_DWORD`) are not
  read. The `HKLM` "Open with" row is `shadowed` when it exists (present or unreadable) and an
  `HKCU` key of the same name exists.
  A `.lnk` working directory still at the target's own directory where the program root moved
  off it is `WorkingDirOutdated` (stale).
- **Child processes.** Only `elevation.rs` starts one: the elevated copy of the running exe
  (`relaunch_self_elevated_with_args`, `apply_elevated`); nothing else here spawns a process.
- **Elevated helper.** `apply_elevated` refuses `User`-scope requests (they run in-process),
  maps `ERROR_CANCELLED` to `Declined`, and on timeout reports `TimedOut` without killing the
  helper (its outcome is unknown). The timeout counts from the helper's start
  (`SEE_MASK_NOASYNC` returns after the UAC prompt), and every error after the launch deletes
  the result file. `run_elevated_helper` never elevates or relaunches; it runs
  `crate::actions::apply` only when the token is elevated, inside
  `crate::actions::run_helper_protocol` (result file created before any action runs).
- **Known debt.** The "Open with" key name is fixed per registry root. Two installs under one
  root share it: the last install wins, and uninstalling the owner removes it even if the other
  install remains (the other install's own uninstall then leaves it alone). A launcher with a
  fallback exe name (`resolve_windows_launcher_target`) still registers under the fixed name.
- **Naming.** The `windows` extern crate is referenced as `::windows` inside this module tree
  (the module name shadows it).

## Editing map
- New or changed registry value: `values.rs` (+ its golden), writer in `registry.rs`. The
  registration actions (`../actions.rs`) delete a record's key tree before writing it.
- What uninstall may delete (ownership evidence): `values.rs` predicates (+ tests), read in
  `registry.rs::remove_record_if_owned`.
- Shortcut location or content: `shortcut.rs`.
- Install-kind rule or path comparison: `mod.rs`.
- Elevation, the UAC launch of the registration helper, the helper entry point: `elevation.rs`
  (protocol: `../actions.rs`).
- What the probe checks per record, or how it finds the owner: `probe.rs` (+ its tables).
