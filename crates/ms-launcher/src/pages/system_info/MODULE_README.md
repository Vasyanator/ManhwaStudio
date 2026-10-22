# Module: crates/ms-launcher/src/pages/system_info

## Purpose
The launcher settings page's "System info" tab: what this machine is (OS, device / chip, CPU,
cores, RAM, every graphics adapter) and which AI acceleration it gets (the effective native
ONNX build and the builds that run here, the PyTorch build the installer would offer, the
PyTorch build the running AI backend reports). A "Copy report" button copies the same content
as plain text for bug reports. Native only (`cfg(not(target_arch = "wasm32"))` in
`pages/mod.rs`; the web build shows a one-line "unavailable" notice instead).

## Architecture
```text
SettingsPageState.system_info: SystemInfoState         (mod.rs, GUI thread)
  show(): first show / Refresh -> spawn_probe(request, ctx)
            worker `launcher-system-info-probe` (blocking, once per refresh):
              ms_sysprobe::system_facts::probe_system_facts()
              + native ONNX (if local_ai.native_onnx is Ok):
                  NativeBuildAvailability::evaluate(NativeHardwareFacts::probe_all())
                  evaluate_native_selection(user_config)
              + PyTorch preflight (if local_ai.backend is Ok):
                  TorchOffer::from_preflight(detect_torch_preflight())
            -> mpsc; WakeOnDrop -> ctx.request_repaint() once on every exit (panic too)
          poll(): try_recv, no repaint while waiting
          per frame: BackendTorch::from_snapshot(handle.health_snapshot())
          report::build_report(probe, backend, local_ai) -> Vec<ReportCard>
          view::draw_cards(cards) / Copy report: ctx.copy_text(report::plain_text(cards))
```

## Files and submodules
- `mod.rs`: `SystemInfoState` (Idle / Running / Ready / Error), header row (hint, Refresh,
  Copy report, copied notice), the worker and its probe body.
- `report.rs`: GUI-free. The probe result types, owner data -> report data reductions
  (`TorchOffer::from_preflight`, `BackendTorch::from_snapshot`), the localized card model
  (`ReportCard` / `ReportItem` / `Tone`), `build_report` and `plain_text`. All wording lives
  here; tests next to it.
- `view.rs`: egui drawing of the cards (launcher info-card frame, fixed label column, bold
  adapter headings, indented details, warning colour from `ms_theme::status::WARNING`).

## Contracts and invariants
- No decision is re-derived here. Facts: `ms_sysprobe::system_facts`. Build availability and
  fallback wording: `ms_settings_ui::native_builds`. Effective selection:
  `ms_native_runtime::evaluate_native_selection`. PyTorch offer: the installer preflight
  (`TorchWheelOption::label`, `rocm_gfx_is_experimental`, `DriverNote::message`). Backend
  build text: `ms_settings_ui::torch_build_text`. Byte sizes: `ms_local_ai::large_models::format_gib`
  (binary GiB; the templates say ГиБ / GiB).
- Local AI gating comes only from `AiBackendHandle::local_ai`: a runtime the platform lacks is
  neither probed nor shown; one off by `--no-ai` is not probed and shown as one line; both off
  by the flag collapse the AI card to one line. The Graphics card is hidden when the platform
  cannot enumerate adapters (`GpuEnumeration::Unsupported`, Android).
- The GUI thread never probes: everything blocking runs on the worker; the backend row reads a
  snapshot clone. The worker repaints once when it sends; the tab does not repaint per frame
  while waiting. A refresh drops the old receiver; a late result is discarded and logged.
- Shown and copied text are the same strings (`plain_text` prints the cards `view` draws).

## Editing map
- New row or wording: `report.rs` (+ keys `launcher.settings.system_info.*` in `en.json` and
  `ru.json`) and a test there.
- New hardware fact: `ms_sysprobe::system_facts` first, then a row in `report.rs`.
- Layout / colours: `view.rs`. Probe lifecycle or what the worker runs: `mod.rs`.
