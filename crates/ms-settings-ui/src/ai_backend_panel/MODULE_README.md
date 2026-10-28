# Module: crates/ms-settings-ui/src/ai_backend_panel

## Purpose
The shared AI-backend settings pane (`ms_settings_ui::ai_backend_panel`), drawn by both the
launcher's settings page and the studio's Settings tab (double-interface pattern,
`egui-docs/04-widgets.md` §7). It renders the shared model limit and three sub-tabs —
«Основной PyTorch» (Python-backend status, process controls, PyTorch device, large models,
diagnostics), «ONNXruntime» (runtime selector, ONNX provider / build / EP / device) and
«Stable-Diffusion.cpp» (sd.cpp build, install / remove, status) — against the app-global
`AiBackendHandle` and a per-UI `AiBackendPanelState`. Its role in the crate is described in the parent `MODULE_README.md`.

## Architecture
```text
draw_ai_backend_panel (mod.rs)
  -> model_manager::draw_shared_model_limit           (desktop; above the tabs)
  -> draw_tab_strip                                   (visible_tabs; only with > 1 tab)
  -> «Основной PyTorch»: backend_process::draw_pytorch_tab
       status / process control / log, draw_torch_device_controls,
       model_manager::draw_model_manager («Большие модели»), draw_torch_build_lines
  -> «ONNXruntime»: runtime_section::draw_ai_runtime_section + onnx_selection::draw_onnx_section
       -> draw_native_build_selection | draw_backend_provider_selection
            -> ort_build: probes, config reads, ORT download, build action, savers
  -> «Stable-Diffusion.cpp»: sdcpp_tab::draw_sdcpp_tab  (desktop)
       ms_sdcpp::snapshot / request_refresh, install / remove job on ms_thread workers
```
The files are split by the part of the AI stack they configure: the Python backend's
PyTorch side, the ONNX selection and its background work, the sd.cpp package, and the shared
model manager. Tabs are listed by platform feature (`PythonBackend`, `SdCppRuntime`), never by
`--no-ai`, which only disables a tab's controls. The selected tab is session state.
Every off-thread write reports its `SettingChange` through the state's `PendingChanges`
channel (`report_landed`); `draw_ai_backend_panel` drains it into the frame's outcome.

## Files and submodules
- `mod.rs`: public entry (`draw_ai_backend_panel`, `AiBackendPanelState`,
  `AiBackendPanelOutcome`, `OnnxConfigRead`, `OrtDownloadStatus`), the change channel
  (`PendingChanges`, `report_landed`) and the option types the sections share
  (`OnnxProviderOption`, `OnnxDeviceOptionUi`); lays the sections out.
- `backend_process.rs`: the «Основной PyTorch» tab — backend status, process control, log,
  PyTorch device selector, large-model section call, PyTorch build line + diagnostics.
- `onnx_selection.rs`: the ONNX section of the «ONNXruntime» tab (desktop and web variants), the Backend-runtime
  unified provider/device combos, the Native-runtime build -> EP -> device selection and the
  pure option / reconcile logic.
- `model_manager.rs` (desktop): the shared model-limit slider (seeded from the config read),
  its saver, the large-model section call.
- `ort_build.rs` (desktop): dylib presence cache, build-action decision and button, ORT
  download and progress, capability probe, config read, build / provider-device savers, ORT
  load-guard reset.
- `runtime_section.rs` (desktop): the Python-backend / native-ONNX runtime selector and saver.
- `sdcpp_tab.rs` (desktop): the «Stable-Diffusion.cpp» tab — build combo (saved through
  `ms_config::save_sdcpp_build`), status from `ms_sdcpp::snapshot` (refreshed on open),
  requirement problem + VC++ runtime link, install / cancel / remove job with progress (Remove
  disabled while `SdCppService::is_generating`); once
  installed: «Найти устройства» (`SdCppService::probe` on a worker, automatic once per open;
  its answer is read back from the status), library version, device combo («Авто» + probed
  `name — description`, saved via `save_sdcpp_device`, warning when the saved device is not in
  the last probe), CPU threads (0 = auto, saved via `save_sdcpp_threads`) and «Остановить
  процесс sd.cpp» (`unload` on a worker) while a worker process runs.
- `tests.rs`: unit tests of the pure logic (desktop test builds only).

## Contracts and invariants
- The public path is `ms_settings_ui::ai_backend_panel::{draw_ai_backend_panel,
  AiBackendPanelState, AiBackendPanelOutcome, OnnxConfigRead, OrtDownloadStatus}`; the
  submodules are private and expose items to each other with `pub(super)` only.
- Types whose private fields several sections read stay in `mod.rs` (descendants see their
  parent's private fields); a type moved into a section needs `pub(super)` fields.
- No config read, probe or write on the GUI thread: they run on named workers in
  `ort_build.rs`, `runtime_section.rs` and `model_manager.rs`; the GUI reads shared slots.
- `cfg(target_arch = "wasm32")` gates: `model_manager`, `ort_build`, `runtime_section`,
  `sdcpp_tab` and `tests` are desktop-only modules; `onnx_selection.rs` carries both section
  variants (the web one keeps its own backend-driven limit slider).
- sd.cpp is installed only by the explicit Install button; the tab never downloads on its
  own. Under `--no-ai` (`AiRouteKind::SdCpp.availability` is `Err`) the tab is shown disabled
  and starts nothing — no probe (not even the automatic one, `auto_probe_due`), install,
  remove or unload. Where the catalog offers no build, the tab says so instead of waiting.
  Device / threads / build saves request an `ms_sdcpp` status refresh, so editors that follow
  the status revision see the new choice. Install / remove / build save run on `ms_thread` workers; `ms_sdcpp::install` /
  `remove` refresh the process-global status themselves.
- A tab label's warning badge comes from its `SettingGroupId` (`AiBackendTab::warning_group`);
  a tab without checked settings has none.
- Build availability is decided by `crate::native_builds`, device ids by `crate::onnx_caps`,
  PyTorch build texts by `crate::torch_build_text`; this directory only draws them.

## Editing map
- To change the pane's layout, the tab strip or which tabs a platform shows, see `mod.rs`.
- To change the backend status, process controls, PyTorch device or build readout, see
  `backend_process.rs`.
- To change the sd.cpp build pick, install / remove, device / threads choice, probe or its
  status lines, see `sdcpp_tab.rs`.
- To change ONNX provider / build / EP / device choice, see `onnx_selection.rs`.
- To change downloads, probes, the build-action button or an ONNX saver, see `ort_build.rs`.
- To change the runtime selector, see `runtime_section.rs`; the model limit, `model_manager.rs`.
