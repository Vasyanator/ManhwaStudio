# Module: crates/ms-settings-ui/src

## Purpose
The settings surface ManhwaStudio's TWO shells share. The launcher's settings page and the
studio's Settings tab render the same General / AI-backend / Tutorials panes and drive the
same AI-backend supervisor, so neither shell may own them. Re-exported by the binary under
the module names its call sites already use (`crate::settings_shared`,
`crate::general_settings_panel`, `crate::ai_backend_panel`, `crate::ai_backend_supervisor`,
`crate::tutorial`).

## Architecture
```text
        launcher settings page          studio Settings tab
                   \                           /
                    \                         /
                     settings_shared  (section registry + SharedSettingsPanels)
                              |
        +---------------------+---------------------+
        |                     |                     |
general_settings_panel   ai_backend_panel      tutorial::settings_pane
        |                     |                     |
   ms_config writers   ai_backend_supervisor   tutorial::{controller, progress}
                              |
                     ms_backend_ipc  ->  the Python AI backend process
```

Each pane uses the project's **double-interface** pattern (`egui-docs/04-widgets.md` §7):
one draw entry the launcher calls with its own frame, one the settings tab calls with
theirs. Adding a pane means adding a `SettingsSectionId` variant and both entries.

Layer position: above `ms-widgets` / `ms-config` / `ms-sysprobe` / `ms-backend-ipc` /
`ms-memory` / `ms-onnx` / `ms-project` (the storage-mode driver) / `ms-os-integration`
(native only: the OS-record probe behind the registration warnings), and above exactly ONE tab crate — `ms-tab-translation`, for
`backend_health` (the health snapshot and probe-command model). The tabs never depend back
on this crate; if a reference in that direction appears, the layering is wrong, not the
dependency list.

## Files and submodules
- `lib.rs`: crate root and module list; the `tutorial` feature gate.
- `settings_shared.rs`: the menu-level layer — `SettingsSectionId`, `sections_for`,
  `title_key`, `SharedSettingsPanels`, which owns the three shared panes, and
  `with_item_badge`, the one layout rule for an item's inline warning badge. The studio
  Settings tab shows six sections: the shared General, AiBackend and Tutorials (the last only
  with the `tutorial` feature) plus its own CanvasRibbon, Typesetting and Hotkeys; the launcher
  adds its own SystemInfo, AiComputations, TorchUpgrade, PythonEnvironment and (Windows and
  Linux only, a `cfg`-gated `SECTIONS` row) SystemRegistration. `SettingsDeepLink`
  is declared in `ms_config::settings_deep_link` (the typing tab requests deep links and may not
  depend on this crate) and only re-exported here.
- `storage_mode_job.rs`: the ONE process-wide Dev/Prod conversion job (slot Idle / Pending /
  Running {done,total} / Finished {outcome}). The startup reconciliation (`main.rs` records it
  pending; the launcher loop or `studio_bootstrap` starts it) and the General pane's switch
  both run `ms_project::storage_mode::convert_globals` through it on a named worker
  (`docstore-reconcile` / `docstore-convert`), so two drivers never run at once. The
  Pending -> Running transition of the reconciliation is claimed in one critical section, so a
  user switch that superseded it always wins. The launcher's chapter conversion registers a
  `ChapterConversionLease` in the same slot: a global job and a chapter conversion exclude each
  other (the global job moves the chapter's target format). Failed documents are reported with
  their real file path (`failed_document_path`, resolved on the worker).
  `backend_autostart_blocked()` (anything Pending/Running or a live chapter lease) is the AI
  backend's autostart gate.
- `storage_mode_setting.rs`: the General pane's storage row (radio Prod/Dev, explanation,
  progress, failure list — the list is reused by the launcher main page). The shown mode is
  `ms_docstore::default_format()`; web builds show an explanation only (Dev-only store).
- `general_settings_panel.rs`: projects root, storage mode, memory profile, UI language, UI scale,
  autosave policy (interval + action threshold, live in `ms_config::autosave_policy`), typesetting
  language, startup monitor. Also owns the process-wide UI-scale slot
  (`ui_scale_percent` / `apply_ui_scale`) every `eframe::run_native` creator applies, and
  the locale-catalog helpers the launcher's first-run language modal reuses.
- `ai_backend_panel.rs`: AI runtime selection, ONNX provider/device/build, model limit,
  backend health readout, ORT crash-guard reset. The provider list is the UNION of the offline
  native set and the providers the backend reports; backend-only providers (e.g. MIGraphX,
  ROCm) stay selectable for backend ONNX. Native build availability (picker groups,
  "(unavailable)" label, auto-download gate) is NOT decided here: it is
  `ms_native_runtime::native_build_fallback_reason` over the caps probe's
  `NativeHardwareFacts`, so the panel shows what the runtime will do. The Backend-branch
  provider list's `native_available` flag is a separate local-capability heuristic (DirectML
  adapter detected, CUDA 12 probe) that only seeds the Backend default provider. Selections persist off-thread through
  `ms_config::save_onnx_provider_device` / `save_onnx_build` / `save_max_loaded_models` and work
  with no backend running.
- `onnx_caps.rs` (native-only): `OnnxCaps` (the probed CUDA / WebGPU / DirectML / OpenVINO
  capabilities and adapter names), `probe_onnx_caps()` (blocking, workers only) and
  `ep_device_ids(ep, &caps)`, the one rule for which `General.ai_onnx_device_id` values an EP
  offers (the AI pane's device combo zips its labels onto those ids), and
  `device_id_offered(ep, id, &caps)`, the one rule for whether a persisted id names an offered
  device — judged on the runtime's own parse (`ms_native_runtime::device_selection_for`), so
  an id the native load accepts (OpenVINO `"GPU.0"`) is never treated as missing. The pane's
  device reconcile and the settings device check both ask it. No UI here.
- `ai_backend_supervisor.rs`: `AiBackendHandle` — the app-global handle both shells drive
  the Python backend process through, plus its health probe and (on Windows) the loopback
  WebSocket handshake. `AiBackendSupervisor` is built once in `run_main` and outlives the
  launcher -> studio switch; the process itself (spawn, stdout/stderr reader threads, runtime
  log) is owned by the `AiBackendProcessRuntime` worker (`spawn_ai_backend_process_worker`). `start_with_autostart_gate` defers the persisted autostart until a
  gate opens (the binary passes `!storage_mode_job::backend_autostart_blocked()`); a user
  Start/Restart/Stop cancels a deferred autostart.
- `settings_warnings/`: per-setting warnings ("!" badges) of the shared panes — the
  GUI-free model (`SettingKey`, `WarningReason` owning the level, `WarningSet`
  aggregation), the native-only checks, the `SettingsWarnings` worker runtime
  (generation-based latest-wins rechecks driven by `SettingChange`) and the badge
  painter. It consumes the detectors above (`check_backend_spawnable`, `onnx_caps`,
  `ms_native_runtime::evaluate_native_selection`, `ms_os_integration::report::probe`) and
  never owns their rules. See its own
  `MODULE_README.md`.
- `tutorial/`: the onboarding subsystem, behind the `tutorial` feature. See its own
  `MODULE_README.md`.

## Contracts and invariants
- **GUI thread.** No pane probes, spawns, downloads or writes config on the GUI thread:
  every such step goes to a named `ms_thread` worker and reports back over a channel.
- **Config writes** go through the `ms_config` section writers (`save_ai_runtime`,
  `save_onnx_provider_device`, `save_onnx_build`, `save_max_loaded_models`,
  `save_text_language`) or through `ms_config::update_user_config_file` (the serialized,
  atomic user-config read-modify-write: `persist_config_keys`, `save_ai_backend_autostart`),
  never by `std::fs` on `user_config.json` here; reads use the ms-config readers
  (`load_user_config`, `ort_load_guard::read_user_config_root`). A malformed file is surfaced
  and never overwritten. That is also why those writers live in `ms-config` and not in the
  studio settings tab. Known exception to the GUI-thread rule: `persist_config_keys` runs
  synchronously on the GUI thread (one tiny write per explicit user action).
- **The `tutorial` feature is NOT inherited.** The root crate forwards it
  (`tutorial = ["ms-settings-ui/tutorial"]`). Enabling one without the other silently drops
  the tours from half the app.
- `tutorial/engine.rs` is ALSO mounted by the `tutorial_test` demo bin through `#[path]`,
  so it must stay dependency-light: egui + std only, no config, no logging, no `t!`.
- **Backend spawn guard.** `start_ai_backend_process` is the single spawn point (autostart,
  Start, Restart). Its preconditions have one owner, `check_backend_spawnable(app_dir)`
  (`BackendSpawnBlocker`: `ai_backend.py` missing; a Python payload without `docstore.py`,
  since a pre-docstore `config.py` would recreate `user_config.json` beside the `.db`, KG-017;
  no resolvable interpreter). It is silent and stats the disk; the spawn adds the error texts
  and the log line, and an inspection caller asks the same function on a worker.
  The autostart gate is polled on the supervisor worker, never on the GUI thread.
- **Outcomes carry changed settings.** `SharedSectionOutcome::changed_settings` (fed by
  `GeneralSettingsOutcome` and `AiBackendPanelOutcome`) lists the checked settings whose
  write ran this frame: the General pane's synchronous saves (projects root, UI language)
  on the click; the AI pane's off-thread saves (runtime, build, EP/device, ORT guard reset)
  only after the write returned, over the state's completion channel (`PendingChanges`),
  drained on every shared-section draw and by `SharedSettingsPanels::take_landed_changes`
  (no draw; the launcher calls it every frame, so a write landing while a launcher-only tab
  or another page is shown is still reported); autostart (persisted by the supervisor worker) on
  the click, carrying the new value. A write is reported whether it succeeded or failed (the
  recheck re-reads the disk). The launcher forwards them to its warning rechecks; the studio
  ignores them.
- **Item badges are launcher-only.** `SharedSettingsPanels::draw` takes
  `warnings: Option<&WarningSet>`. The studio passes `None`, which must draw the exact
  pre-badge widget tree; every badged item goes through `settings_shared::with_item_badge`
  (or appends `item_warning_badge` inside an existing row), never a bespoke layout.
- No literal user-visible strings: `t!` / `tf!` / `tp!` only, and a stable `id_salt` on any
  localized label.
- Native-only pieces (ORT runtime, monitor list, backend subprocess) keep their
  `cfg(not(target_arch = "wasm32"))` gates INSIDE the files; the matching crates are
  declared in the target table of `Cargo.toml`.

## Editing map
- To add or change a settings warning (check, severity, badge), see `settings_warnings/`;
  to badge a new item or report a new `SettingChange` from a pane, see the pane file and
  `settings_shared.rs` (`with_item_badge`, `SharedSectionOutcome`).
- To change the storage-mode switch UI, see `storage_mode_setting.rs`; its job lifecycle,
  `storage_mode_job.rs`; which documents convert and in what order, `ms-project`'s
  `storage_mode.rs`.
- To add a settings section, see `settings_shared.rs` (`SettingsSectionId`, `sections_for`,
  `title_key`) and add both halves of the double interface in the new pane.
- To change what the AI pane offers or reports, see `ai_backend_panel.rs`; which device ids
  an EP offers or what the capability probe gathers, `onnx_caps.rs`; how the backend process
  is started, watched or stopped (and what blocks a spawn), `ai_backend_supervisor.rs`.
- To change a persisted settings key, add the writer in `ms-config` first, then call it here.
- To change the tours, see `tutorial/` (`id.rs` for the stable keys, the per-surface step
  scripts live with their surfaces in the launcher).
