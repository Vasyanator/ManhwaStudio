# Module: src/tabs/settings

## Purpose
Settings tab implementation for user and project options that affect the editor runtime: general
launcher settings, global memory profile, shared canvas/ribbon behavior, AI backend process/device
controls, and hotkey overrides.

## Architecture
`SettingsTabState` in `mod.rs` owns pane selection, shared settings snapshots, the hot-applicable
`MemoryManager` binding, and bindings to shared models. The AI backend pane no longer owns the
backend process: it holds a cloned `AiBackendHandle` and renders the shared widget from
`crate::ai_backend_panel`. Pane files render focused UI sections through methods on
`SettingsTabState`.

Settings writes are offloaded to worker threads or a coalescing save worker. The GUI thread updates
the in-memory `SharedCanvasSettings`, sends save requests, polls shared snapshots, and forwards
backend process/probe commands through the `AiBackendHandle`.

The `ai_backend.py` process and the health/device probe are owned **app-globally** by
`crate::ai_backend_supervisor` (created in `run_main` before the launcher/studio loop), so the
backend starts in the launcher (per the autostart toggle) and survives launcher↔studio transitions.
Both the studio settings tab and the launcher settings page render the same
`ai_backend_panel::draw_ai_backend_panel` against that shared handle. The backend speaks the framed
IPC protocol over the AF_UNIX socket at `backend_ipc::backend_socket_path()`; the supervisor passes
that path to Python with `--socket`. There is no free-port reservation and no HTTP server.

## Files and submodules
- `mod.rs`: `SettingsTabState`, pane routing, canvas settings binding, the shared `AiBackendHandle`,
  typing-layout persistence helper, and the coalesced canvas settings save worker. (The
  projects-dir + memory-profile editing/persistence moved to the shared
  `crate::general_settings_panel` widget; the `user_config.json` write lock moved to `config`.)
  (The backend process worker + autostart persistence now live in `crate::ai_backend_supervisor`.)
  Also hosts the `user_config.json` writers for the AI runtime selector, the unified ONNX selection,
  and the ONNX Runtime SIGILL load-guard: `save_ai_runtime` (writes `General.ai_runtime` and sets
  `General.ai_runtime_configured=true`, marking the runtime as an explicit user choice so the native
  default no longer applies),
  `save_onnx_provider_device` (writes `General.ai_onnx_provider`/`ai_onnx_device_id` + the
  `*_configured` flags, the SAME keys the backend uses, so one selection drives both runtimes),
  `save_onnx_build` (writes `General.ai_onnx_build`, the native-only build slug picking the onnxruntime
  binary; wired to the "Билд" selector),
  `save_max_loaded_models` (writes `General.ai_max_loaded_models` as an integer), and
  `mark_ort_load_attempted` / `mark_ort_load_succeeded` / `reset_ort_load_guard` (mutate
  `General.ort_load_state[scope]`, where `scope` is `provider[:device]@version`). The three guard writers fsync the file after writing so the
  aborted-attempt marker survives an uncatchable SIGILL during onnxruntime load (and the marker write
  also fsyncs the parent directory on a first-ever create, Unix-only); all are synchronous
  read-modify-write helpers meant to run off the GUI thread. ALL `user_config.json` RMW writers in
  this module (`save_*` + `write_ort_load_state`) serialize on the process-wide write lock, now
  `config::lock_user_config_write()` (moved to `config` so the shared general-settings widget serializes
  on the same lock), so concurrent background/GUI-thread savers cannot interleave read/write and lose an
  update (which could drop the just-written `attempted:true` SIGILL marker or clobber settings).
  `save_ai_runtime` is wired to the
  "Рантайм ИИ" selector in `ai_backend_panel`; the guard writers are wired to `native_runtime`'s ORT
  load path and the "Повторить попытку ORT" reset control.
- `general.rs`: thin studio wrapper for the general pane. Enforces the studio-only vertical
  typing-panel layout (persisted off-thread via `save_typing_panel_layout`), then delegates the
  projects-directory editor, global memory-profile combo, and the UI-language selector to the shared
  `crate::general_settings_panel` widget and applies the memory-profile runtime effect from its
  outcome. Projects-dir + memory-profile + UI-language persistence live inside that shared widget,
  not here. The UI-language selector (a `WheelComboBox`) lists the locales found in the on-disk
  `locale/` folder — scanned ONCE at widget construction, never per frame — each shown by its
  `_meta.name`; changing it persists `General.ui_language` (synchronously, like the memory-profile
  write) and live-installs that locale's catalog via `crate::locale_store::install_ui_locale`
  (no restart).
- `canvas_ribbon.rs`: shared ribbon/canvas pane for bubble type defaults, aside/on-top layout,
  spellcheck word lists, bubble status rules, and related `SharedCanvasSettings` fields.
- `typesetting.rs`: "Тайп" pane for text-typesetting options: the app-wide
  hanging-punctuation list editor (`TextTab.hanging_punctuation`, applied live via
  `crate::text_punctuation` and persisted through `save_hanging_punctuation` in `mod.rs`)
  the "Поворот Ctrl+колесо" chooser (`TextTab.rotation_ctrl_wheel_mode`, applied live
  via the `crate::tabs::typing::rotation_ctrl_wheel` global and persisted through
  `save_rotation_ctrl_wheel_mode` in `mod.rs`; read by the typing tab's Ctrl+wheel handler),
  the typesetting-language selector — two `WheelComboBox`es (`ScriptGroup` then the concrete
  `TextLanguage` within it; changing the group selects that group's first language) — applied live
  via `ms_text_util::language::set_text_language` (the typing tab's `panel/facade.rs` observes
  `text_language()` each frame and re-runs font-coverage classification off-thread) and persisted as
  `TextTab.text_language` (the `lang.tag()`) through `save_text_language` in `mod.rs` on a background
  thread; the process-global atomic is the single source of truth, so no selection state is stored on
  `SettingsTabState`,
  the per-effect-kind default-parameter editor (`crate::tabs::typing::EffectDefaultsEditorState`
  held on `SettingsTabState`, rendered via its `ui()`; a self-contained typing-panel widget that
  owns its own persistence to `TextTab.effect_defaults`, so settings needs no access to the private
  effect model), and the "Настройки шрифтов" block
  (`crate::tabs::typing::FontSettingsEditorState`, same double-interface pattern): it lists the app's
  fonts in three categories rendered in their own typefaces and imports/removes system fonts via the
  runtime-global imported-fonts store, loading font lists off the GUI thread. Both blocks are wrapped
  in collapsed `CollapsingHeader`s.
- `ai_backend.rs`: AI backend pane UI for health display, process start/stop/restart/autostart,
  device/provider selection, max loaded models, and CUDA/ROCm diagnostics. It forwards to the shared
  `crate::ai_backend_panel::draw_ai_backend_panel`, which also hosts: the "ONNX-инференс" runtime
  selector (backend Python / native ONNX, persisted to `General.ai_runtime`; Torch always runs on the
  backend); the OFFLINE-capable ONNX selection, RUNTIME-BRANCHED — under Native the BUILD-based combos
  (Билд → EP → Устройство): the "Билд" combo lists the `onnx_runtime::builds` catalog grouped by
  availability (Базовые/Специфичные/Недоступные, QNN display-only), the EP combo comes from the selected
  build, and the device combo adapts per EP (incl. OpenVINO device-type strings); an available build's
  dylib auto-downloads and the build-action button is Retry / "Загрузить другую сборку ort" (force
  download + `reset_load_latch`) / "Перезапустите программу" per `native_runtime::ort_dylib_committed()`
  + `active_build()` (a committed dylib can't hot-swap). Under Backend the UNIFIED provider/device combos.
  The model-limit slider is shared. Selecting persists the unified keys via `save_onnx_build` /
  `save_onnx_provider_device` / `save_max_loaded_models` off-thread AND, when connected, pushes
  `device.set`; the onnxruntime auto-download progress bar renders via
  `onnx_runtime::resolve_or_download_ort_dylib(build)` on a worker thread; the same-build "Повторить
  попытку ORT" retry calls `reset_ort_load_guard` for the effective `provider[:device]` scope +
  `native_runtime::reset_load_latch`. The PyTorch device combo stays backend-gated. All ONNX native bits
  are desktop-only (`#[cfg(not(wasm32))]`); config/caps/dylib-presence are read once off the GUI thread
  into the panel's scratch state. On wasm the ONNX combos remain backend-driven.
- `hotkeys.rs`: configurable hotkey list, live shortcut capture, reset/clear actions, and
  `user_config.json` override persistence.
- `tutorials.rs`: thin "Обучение" pane that delegates to the surface-agnostic
  `crate::tutorial::draw_tutorials_pane` (same double-interface pattern as `ai_backend.rs`), so the
  studio and launcher expose the identical tutorial-replay UI. Operates on `SettingsTabState`'s own
  `tutorial_progress` handle (loaded here; the studio has no tutorial controller yet, so resets
  persist to config and take effect on the next launcher run / future studio tutorials).

## Contracts and invariants
- Do not block the GUI thread with file writes, Python process work, backend probes, or command
  output reads. Use the existing workers and command channels.
- Runtime path and Python command construction must go through `config` and `python_manager`.
- Shared canvas settings changes must update the local snapshot, publish to bound shared models,
  and persist through the settings save worker.
- Memory profile changes must update the shared `MemoryManager` immediately and persist
  `General.memory_profile` in `user_config.json` without editing project canvas settings.
- AI backend controls must respect `--no-ai`: disabled state should not start or command the
  process, and health/probe UI should show the disabled snapshot.
- Hotkey capture treats `Esc` as cancel; modifier-only bindings are explicit `Ctrl`/`Alt`/`Shift`
  choices, not accidental raw key captures.
- Persisted settings errors should be surfaced or logged with context; do not silently replace real
  failures with fake success.

## Editing map
- To add or rename a settings pane, update `SettingsPane`, pane switcher/routing in `mod.rs`, and
  add a focused pane file if it grows beyond trivial UI.
- To change shared canvas/ribbon settings, edit `canvas_ribbon.rs` and the save/apply helpers in
  `mod.rs`.
- To change AI backend process controls, process logs, autostart, or device/probe commands, edit
  `ai_backend.rs` and the worker functions in `mod.rs`.
- To change the projects directory, the memory profile, or the UI-interface language, edit the shared
  `crate::general_settings_panel` widget (used by both the studio pane and the launcher); the studio
  `general.rs` is only a thin wrapper that adds the typing-panel-layout enforcement and applies the
  memory-profile runtime effect. The UI-language list is scanned from `locale/` once at construction;
  installation goes through `crate::locale_store::install_ui_locale`.
- To change text-typesetting options (hanging punctuation, Ctrl+wheel rotation mode, typesetting
  language), edit `typesetting.rs` and the matching persistence helper in `mod.rs`
  (`save_text_language` for the language). Runtime globals for these live outside settings
  (`crate::text_punctuation`, `crate::tabs::typing::rotation_ctrl_wheel`,
  `ms_text_util::language`).
- To change the collapsed effect-defaults / font-settings blocks in the "Тайп" pane, edit their
  widgets in `src/tabs/typing/panel/` (`effect_defaults.rs` / `font_settings.rs`); `typesetting.rs`
  only wraps each `ui()` in a `CollapsingHeader`.
- To change configurable shortcut UI or persistence, edit `hotkeys.rs` and coordinate with
  `src/input_manager_v2.rs`.
