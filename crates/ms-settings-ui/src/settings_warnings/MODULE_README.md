# Module: crates/ms-settings-ui/src/settings_warnings

## Purpose
Per-setting warnings of the shared settings panes: a "!" badge on a settings item whose
current value cannot work (Red) or does not work as the user probably expects (Yellow),
aggregated up to its settings tab and to the launcher's main-menu Settings button. This
directory owns the model, the checks, the worker and the badge painter; the panes (item
badges, change reporting) and the launcher (worker lifecycle, tab / menu badges) only call
its public surface.

## Architecture
```text
launcher entry ──start(ctx)──> SettingsWarnings ──CheckRequest{key->generation, ctx}──> worker "settings-warnings"
pane write landed ─recheck(changes)─┘      ^                                               | coalesce -> run_checks per ctx
                                           └──────── CheckBatch{(key, generation, outcome)} ┘ + request_repaint
GUI frame: poll() -> WarningSet -> item_warning_badge / section_level / overall_level -> badges
```
- **Model** (`model.rs`, wasm-clean, no egui): `SettingKey` -> `SettingLocation { section,
  group }`; `WarningReason` -> `WarningLevel` (owner of severity) and `message()` (localized
  at draw time); `SettingChange::affected_keys` (the recheck table); `WarningSet`.
- **Checks** (`checks.rs`, native-only): the registration family first (one OS probe,
  independent of config), then one raw config read per run, then per requested key the
  existing detectors, then a pure decision function per check.
- **Runtime** (`runtime.rs`): one long-lived worker per `SettingsWarnings`; generations make
  every evaluation unit latest-wins; `coalesce` keeps each key's own request context.
- **Badge** (`badge.rs`): the only egui code.

## Files and submodules
- `mod.rs`: module root and the public re-exports.
- `model.rs`: keys, locations, levels, reasons, changes, `WarningSet` aggregation.
- `checks.rs`: fact gathering (`run_checks`) and the pure rules (`projects_root_reasons`,
  `ui_catalog_reasons`, `backend_reasons`, `native_checks_apply`, `native_family_reasons`,
  `registration_reasons`). Also the one owner of the record kind -> warning key / record name
  mappings (`registration_key`, `registration_record`, public on Windows and Linux), which the
  launcher's System registration tab uses for its row badges and titles.
- `runtime.rs`: `SettingsWarnings` (`start`, `disabled`, `recheck`, `poll`, `set`),
  `CheckContext`, and the worker side (`coalesce`, `worker_loop`, generic over the run
  function so tests drive it by hand).
- `badge.rs`: `paint_warning_badge`, `paint_corner_badge`, `item_warning_badge`,
  `WarningLevel::color`.

## Contracts and invariants
- **Checks map.** ProjectsRoot (Yellow: a non-default folder is missing, or the path is not a
  directory; a missing DEFAULT root is clean). UiLanguage (Yellow: the on-disk catalog does not
  load -> embedded or English). AiRuntime (Red: runtime `Backend` and the backend cannot
  spawn). BackendAutostart (Red: autostart on and the backend cannot spawn, except a missing
  Python while `AiInstallType::None`). OnnxBuild (Red: the EFFECTIVE build is not shipped
  here; else Yellow: CPU fallback for a build-level cause). OnnxProvider (Yellow: CPU fallback
  for an EP-level cause). OnnxDevice (Yellow: no fallback, the EP consumes the id, and the
  persisted id does not name an offered device). OrtCrashGuard (Red: the guard of the scope
  the next native load reads is `Suspect`). Every AI check is silent under `--no-ai`; the native family is silent unless the
  runtime is `Native`. Registration (Windows: StartMenu, ProgramEntry — App Paths folds into
  it —, OpenWith; Linux: StartMenu, OpenWith; Yellow `RegistrationBroken`): a record that is
  `badge_worthy` AND has an action in `allowed_actions`, so every badge is clearable from the
  System registration tab (Linux system-wide entries and unreadable records never badge).
  Silent under `--ignore-installed` (`CheckContext::registration_checks == false`).
- **One owner per rule; this module only consumes.** Shipping: `build_shipped_here`; CPU
  fallback: `evaluate_native_selection` (`NativeSelectionReport::fallback`); whether the device
  id is used: the report's effective `device`; whether the persisted id names an offered
  device: `onnx_caps::device_id_offered` (over the runtime's own id parse, so OpenVINO
  `"GPU.0"` is valid), with `ep_device_ids` only for the count in the message;
  guard scope: `ms_native_runtime::next_load_scope_key`; broken vs stale OS record:
  `ms_os_integration::report::badge_worthy` / `Defect::severity`; actionable record:
  `ms_os_integration::actions::allowed_actions`;
  spawnability: `check_backend_spawnable`; catalog loading: `locale_store::probe_disk_catalog`;
  severity: `WarningReason::level`. Never add a second copy of any of them here.
- **Never pin the native selection.** The worker must not call `native_load_scope_key()` or
  anything that initializes the process selection cache; it uses the uncached
  `evaluate_native_selection` and the side-effect-free `next_load_scope_key()`.
- **Guard scope rule.** The OrtCrashGuard check reflects the scope the NEXT native load of
  this process reads: the committed scope when the process already resolved its selection
  (it cannot change without a restart, so the configured one would only apply at the next
  launch), else the configured effective scope. The AI pane's Retry worker resets exactly
  that scope through the same `next_load_scope_key` and no longer pins the selection, so a
  Red badge is always cleared by its own button and a build changed after Retry is
  rechecked against its own guard.
- **No reconciliation.** The worker reads `AiInstallType` from config; reconciling it stays
  with its owner in the launcher.
- **Registration is config-independent.** `run_checks` probes the OS records (one
  `report::probe` per run, of `CopyIdentity::current(None)`: the version only feeds a stale
  defect) BEFORE the config read, so an unreadable config skips only the config-backed keys.
  An unresolvable executable skips the registration family (logged).
- **Platform-filtered keys.** `SettingKey::ALL` = the portable keys + `REGISTRATION_FAMILY`,
  which is cfg-filtered (Windows 3, Linux 2, elsewhere 0), so macOS and the web build never
  request a registration key. `RegistrationRecord` / `RegistrationProblem` are wasm-clean
  mirrors of the native `RecordKind` / broken `Defect`s; their texts are the
  `launcher.sysreg.*` keys shared with the launcher tab, localized at draw time.
- **Latest wins per key.** `recheck` stamps every affected key with one fresh generation; a
  result is applied only when its generation equals the latest request for that key.
  `Skipped` (config unreadable, I/O error) keeps the previous entry.
- **Context per key.** The worker never merges contexts across requests: each key is checked
  with the context of the request holding its max generation (one `run_checks` per distinct
  context). An explicit `SettingChange::BackendAutostart(v)` is latched in
  `SettingsWarnings` and overrides the snapshot-derived autostart of every later request:
  the supervisor snapshot lags the toggle, and the pane's toggle is the only autostart
  writer while the instance lives.
- **Threading.** Checks block (disk, `nvidia-smi`-class probes): worker only. `poll` and the
  badges are GUI-thread safe. The worker exits when the owner drops (request channel
  disconnect) or its result cannot be delivered.
- **wasm.** `checks.rs` is compiled out; `SettingsWarnings::start` returns an inert instance.
- **Groups.** `SettingLocation::group` is reserved for collapsible lists; `SettingGroupId`
  is uninhabited today. Aggregation goes through one `worst_where` predicate, so a
  `group_level` is a one-liner when the first group exists. Every key must stay in General,
  AiBackend or SystemRegistration (sections the launcher lists without a dynamic hide on the
  platforms that check their keys): a unit test pins it.
- **Badges.** Painting registers no hitbox; `item_warning_badge` allocates exactly one
  hover-sensed square and nothing when given `None` (the studio) or a clean key.
- **i18n.** Messages are `settings.warnings.*_tooltip`, fallback fragments
  `settings.warnings.fallback_reason.*_label`, registration record names and defect fragments
  `launcher.sysreg.*` (shared with the launcher tab), in `en.json` and `ru.json`. Button / option
  names are substituted from their own keys, never copied. Logs are English (`Debug` form).

## Known gaps
- The fact gathering (`run_checks`, `gather_*`, `check_projects_root`) is untested glue: it
  probes real hardware and the filesystem. The rules it feeds are unit-tested.

## Editing map
- To add a checked setting: a `SettingKey` variant + `location`, its reasons in
  `WarningReason` (+ `level` / `message` and i18n keys), its gathering and pure rule in
  `checks.rs`, and the `SettingChange` that rechecks it.
- To change a severity: `WarningReason::level` only. Which OS records are broken is not
  decided here: `ms_os_integration::report` (`Defect::severity`, `badge_worthy`).
- To add a collapsible group: a `SettingGroupId` variant and the key mapping in
  `SettingKey::location`.
- To change the badge look: `badge.rs`.
