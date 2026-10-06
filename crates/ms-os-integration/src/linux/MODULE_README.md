# Module: crates/ms-os-integration/src/linux

## Purpose
The Linux records of a program copy: the `manhwastudio_rs.desktop` entry (application menu and
"Open with") and its 512 px icon — the exact text, the ownership rule, the startup writer and the
read-only registration probe.

## Architecture
- `desktop_entry.rs` (pure): `linux_desktop_entry_text`, the `[Desktop Entry]` group parser
  `main_group_entries`, `entry_owner` (ownership key, else legacy `Exec`), `resolve_program`
  (bare name -> `$PATH` lookup, pure over the `$PATH` value and an "is executable" oracle),
  `resolved_entry_owner` (the owner after that lookup), `decide_startup`,
  `desktop_mime_types` / `desktop_mime_type_list` (from `ms_config::single_image::INPUT_FILE_TYPES`)
  and the escaping helpers.
- `xdg.rs` (`target_os = "linux"`): `DesktopDirs` (`$XDG_DATA_HOME` / `$XDG_DATA_DIRS`, pure over an
  env closure), `entry_path_under` / `icon_path_under`, `same_executable`, `ensure_at_startup`
  (atomic writes), `resolve_program_in_process_path` (the production resolver), `write_entry`
  (the unconditional write of this copy's entry + icon for the
  registration actions; no refresh) and `refresh_linux_desktop_database`.
- `probe.rs`: `evaluate_desktop_entry` (pure judge over `EntryOracles` -> `[StartMenu, OpenWith]` rows) and the
  readers `probe_records` / `probe_records_in` (`target_os = "linux"`).

## Contracts and invariants
- Ownership: `X-ManhwaStudio-Exe=` equal to `CopyIdentity::exe`, else (legacy entry) the first
  `Exec=` argument; a bare program name (no `/`, a spec-legal `$PATH` lookup) is resolved through
  `$PATH` (`resolved_entry_owner`), and one found nowhere is "no owner" (startup leaves the entry
  alone, the probe reports `Unreadable(NoOwner)`, never a badge); `Exec=` / `TryExec=` are
  resolved the same way before they are compared with the owner. Paths compare canonicalized when
  both exist, lexically otherwise (`xdg::same_executable`). The writer and the probe use the same
  rule.
- An owner that is not this copy and whose file name is not `identity::LINUX_EXE_NAME` is a
  foreign program: the probe reports both rows `OursBroken` with `ForeignProgram` (Repair offered,
  confirmed); the startup policy still leaves such an entry alone (it has another owner).
- The pure judge never uses host path predicates (`Path::is_absolute`): an absolute `Icon=` is
  one starting with `/`, so the judge behaves the same in a Windows host test build.
- The startup policy never takes over another copy's entry; only an explicit registration action
  (`crate::actions::apply` -> `xdg::write_entry`) overwrites it. Removing the entry keeps the icon.
- Writes only under `$XDG_DATA_HOME`; `$XDG_DATA_DIRS` copies are read-only `Machine` rows of the
  probe (a missing system copy yields no row; one behind a higher-precedence copy is `shadowed`).
- `Path=` is `CopyIdentity::program_root` (the repository root for a repository build, see
  `../copy_identity.rs`), so the menu and "Open with" launches of a build start in its checkout.
  The probe compares it with that root for this copy and with the repository root for another
  copy that is a repository build; `Path=` still at the owner's exe directory is
  `WorkingDirOutdated` (stale), and startup rewrites this copy's entry anyway.
- The probe judges each entry twice: the menu row (launch values + icon) and the "Open with" row
  (launch values + `MimeType=` coverage + an `Exec=` with a file field code). A legacy entry
  without `TryExec=` / `Path=` / ownership key is stale, never broken.
- `desktop_entry.rs` and the pure part of `probe.rs` are compiled into native host test builds on
  every host; all filesystem and process I/O is `target_os = "linux"` only. Tests use temporary
  directories under `std::env::temp_dir()`, never the real home.

## Editing map
- Entry keys or MIME list: `desktop_entry.rs` (+ its byte-exact golden).
- Ownership rule or startup decision: `entry_owner` / `resolve_program` / `resolved_entry_owner`
  / `decide_startup` in `desktop_entry.rs`.
- XDG resolution, write paths, refresh: `xdg.rs`.
- What the probe checks: `evaluate_desktop_entry` in `probe.rs`; severity of a defect lives in
  `../report.rs` (`Defect::severity`), never here.
