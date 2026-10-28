# Module: crates/ms-native-package/src

## Purpose
The one owner of the "pinned native package" transaction: a directory holding the files
extracted from one or more pinned archives (native shared libraries such as onnxruntime or,
later, stable-diffusion.cpp), trusted only through a completion marker. It downloads each
archive, verifies it, stages the wanted members, publishes them, writes the marker, answers
"is it installed", and removes a package. Level 3: GUI-free, no i18n, no config, native-only
(the crate is empty on wasm, like `ms-download`).

## Architecture
```
ensure_installed(env, layout, recipe, busy_key, progress)
  validate marker + file names (plain, distinct)   [InvalidFileName / DuplicateFileName]
  Probing; marker == recipe header && every member at its size? -> Done   [AlreadyInstalled]
  BusyGuard::acquire_waiting(busy_key)                    [WaitingForOtherDownload, cancel]
  re-probe -> Done                                                        [InstalledByOther]
  create dir, dir/.download; remove the marker + fsync dir (BEFORE any member changes)
  for each source in order (fetch.rs):
      claim .download/<file>.part   (.part.id = "<url>\n<sha256>\n"; foreign -> delete)
      ms_download::fetch_verified(SizeSha256)   [Downloading / Retrying / Verifying]
      Extracting: ms_download::unpack::stage_members (all or nothing, <dest>.tmp + fsync)
      rename every <dir>/<name>.tmp -> <dir>/<name> in member order; fsync dir
      delete the archive
  write the marker atomically (ms_atomic_file, ContentsAndDirectory) -> Done [Installed]
```
- `lib.rs`: the public types (`PackageLayout`, `PackageRecipe`, `PackageSource`,
  `PackageMember`, `FetchEnv`, `PackageStage`, `PackageProgress`, `InstallOutcome`,
  `DOWNLOAD_DIR`, re-exported `ArchiveKind`) and the entry points `flatten_member_name`,
  `is_installed`, `ensure_installed`, `remove_installed`.
- `marker.rs`: marker text (recipe header + member lines), the trust check, durable removal,
  atomic publication, directory fsync.
- `fetch.rs`: one source — `.part` identity claim, verified download, extraction + renames,
  progress throttle (512 KiB) and `ms-download` event mapping.
- `error.rs`: `PackageError`, `PackageIoOp`.

## Files and submodules
- `lib.rs`: API and the transaction order. Edit for a new step or a new entry point.
- `marker.rs`: the marker format and trust rule. Never change the byte format (see contracts).
- `fetch.rs`: per-source download / extraction / publication and progress mapping.
- `error.rs`: typed failures; a new variant must be mapped by every consumer's error mapping
  (`ms_onnx_runtime::map_package_error` has an exhaustive table test).
- `tests.rs`: scripted-server tests (`ms_download::test_support::ScriptedFetcher`), no network.

## Contracts and invariants
- Marker format (persistence contract shared with caches of earlier program versions):
  `<marker_format>\n`, `<key> <value>\n` per recipe field in order, `source <url> <sha256>\n` per
  source in fetch order, then `member <file name> <size> <sha256>\n` per published member in
  publication order (sources in order, members in order). Trusted only when it starts with
  exactly the current recipe header, lists exactly the recipe's member names and every member is
  a regular file of the recorded size (sizes only; the sha256 is diagnostic). A consumer that
  changes its meaning bumps ITS format line; this crate never rewrites the format.
- Staging names are a persistence contract too: `dir/.download/<file>`, `<file>.part`,
  `<file>.part.id` (identity bytes `<url>\n<sha256>\n`), member temporaries `<name>.tmp`.
- Ordering: the marker is removed durably before the first member rename and written atomically
  after the last; the last member of the last source is published last. A crash midway leaves no
  marker, and the next call repairs the set — from the still-staged verified archive when it
  exists (`fetch_verified` keeps a matching `dest`), else by downloading again.
- Network rule (`PROJECT_RULES.md` "Network downloads"): `ms-download`'s retry, resume, timeout
  and cancel behaviour; cancel and transient failures keep the `.part`; a sha256 mismatch deletes
  it (`ChecksumMismatch`); a `.part` of another recipe is discarded, never resumed.
- One install per package per process: the caller's `busy_key` (process-wide namespace shared
  with `ms_download::busy`; never a `dest:` id). The second caller waits, then takes the fast
  path. `remove_installed` takes the same key.
- Path safety, checked before any I/O by every entry point: the marker name and every member
  and archive name are one plain path component other than `.download` (members / archives
  also not the marker) — `InvalidFileName`; archive names (with their `.part`, `.part.id`,
  `.part.identity` staging names) are unique across sources and member names (with their
  `.tmp` temporaries) unique across ALL sources — `DuplicateFileName`. `flatten_member_name`
  gives a member's file name; a consumer with clashing basenames must rename them itself.
- Threading: every entry point does blocking disk (and network) I/O — worker thread only.
- Logging: `ms_log::runtime_log` lines carry the caller's `log_tag` + `log_context`; the
  consumer logs its own outcome lines. No secrets in logs.
- Errors: typed `PackageError` with English `Display`; consumers map variants (operation +
  path + source) onto their own localized texts.

## Editing map
- A new consumer: build a `PackageLayout` (own marker file + format line), a `PackageRecipe`
  (fields that identify the package, sources with pins and members), a unique busy key, call
  `ensure_installed` / `is_installed` / `remove_installed` from a worker and map `PackageError`.
- Archive formats and member staging: `ms_download::unpack` (not here).
- Retry / resume / verification: `ms-download` (not here).
- Known gap: the waifu2x installer and the external model catalog still have their own copies
  (`dev-docs/known_gaps.md` KG-035).
