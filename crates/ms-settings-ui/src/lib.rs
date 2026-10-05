/*
FILE OVERVIEW: crates/ms-settings-ui/src/lib.rs
Crate root of `ms-settings-ui`: the settings surface shared by BOTH application shells —
the launcher's settings page and the studio's Settings tab. The binary re-exports every
module under the name its call sites already use (`crate::settings_shared`,
`crate::general_settings_panel`, `crate::ai_backend_panel`, `crate::ai_backend_supervisor`,
`crate::tutorial`), so no path in `src/` had to change.

Main modules:
- `settings_shared`: the menu-level layer — the section registry
  (`SettingsSectionId` / `sections_for` / `title_key`) and `SharedSettingsPanels`, the
  container owning the three double-interface panels below.
- `general_settings_panel`: the "Общие" pane (projects root, storage mode, memory profile,
  UI language, UI scale, typesetting language, startup monitor) plus the process-wide
  UI-scale slot every `eframe::run_native` creator applies.
- `storage_mode_job` / `storage_mode_setting`: the process-wide Dev/Prod conversion job
  (named worker running `ms_project::storage_mode::convert_globals`) and its pane row.
- `ai_backend_panel`: the AI-backend pane (runtime selection, ONNX provider/device/build,
  model limit, health readout, ORT crash-guard reset).
- `ai_backend_supervisor`: the app-global handle both shells drive the Python AI backend
  through (`AiBackendHandle`), including its health probe, and the backend spawn
  preconditions (`check_backend_spawnable`).
- `onnx_caps` (native-only): the probed ONNX capabilities (`OnnxCaps`, `probe_onnx_caps`) and
  the EP device-id rule (`ep_device_ids`) the AI pane and inspection callers share.
- `settings_warnings`: the per-setting warnings of the shared panes — keys, reasons and
  aggregation (`WarningSet`), the checks worker (`SettingsWarnings`) and the "!" badge.
- `tutorial` (feature-gated): the onboarding overlay engine, the per-surface controller,
  the persisted progress and the shared "Обучение" pane.

Why a crate: these four surfaces are needed by the launcher AND by the studio settings
tab, so neither may own them. They sit ABOVE the primitive layers (`ms-widgets`,
`ms-config`, `ms-sysprobe`, `ms-backend-ipc`) and depend on exactly one tab crate,
`ms-tab-translation`, for the backend-health model — the tabs never depend back.
*/

#![warn(clippy::all)]

// `t!` / `tf!` / `tp!` for every user-visible string of the settings surfaces.
#[macro_use]
extern crate ms_i18n;

pub mod ai_backend_panel;
pub mod ai_backend_supervisor;
pub mod general_settings_panel;
// Probed ONNX capabilities and the device-id rule over them (native-only probes).
#[cfg(not(target_arch = "wasm32"))]
pub mod onnx_caps;
pub mod settings_shared;
// Per-setting warnings ("!" badges): GUI-free model, native-only checks, the worker
// runtime and the badge painter.
pub mod settings_warnings;
// The process-wide Dev/Prod storage conversion job (startup reconciliation and the
// General pane's switch share it) and the pane row that drives it.
pub mod storage_mode_job;
pub mod storage_mode_setting;

// The onboarding tutorial subsystem, behind the `tutorial` feature exactly as in the
// binary (off by default: the controller and its `mark` sites stay compiled but inert,
// and all autoplay/overlay rendering plus the settings pane are gated out). The root
// crate's own `tutorial` feature forwards to this one — they must never be enabled
// separately, or the tours silently disappear from one half of the app.
//
// The engine file itself is ALSO mounted by the `tutorial_test` demo bin through
// `#[path]`, which is why it must stay dependency-light (egui + std only) and why it
// keeps compiling for that bin regardless of this feature.
#[cfg(feature = "tutorial")]
pub mod tutorial;
