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
  projects-dir/typing-layout persistence helpers, and the coalesced canvas settings save worker.
  (The backend process worker + autostart persistence now live in `crate::ai_backend_supervisor`.)
- `general.rs`: general pane UI, including global memory profile, projects directory, and the
  current typing panel layout persistence behavior.
- `canvas_ribbon.rs`: shared ribbon/canvas pane for bubble type defaults, aside/on-top layout,
  spellcheck word lists, bubble status rules, and related `SharedCanvasSettings` fields.
- `typesetting.rs`: "Тайп" pane for text-typesetting options: the app-wide
  hanging-punctuation list editor (`TextTab.hanging_punctuation`, applied live via
  `crate::text_punctuation` and persisted through `save_hanging_punctuation` in `mod.rs`)
  the "Поворот Ctrl+колесо" chooser (`TextTab.rotation_ctrl_wheel_mode`, applied live
  via the `crate::tabs::typing::rotation_ctrl_wheel` global and persisted through
  `save_rotation_ctrl_wheel_mode` in `mod.rs`; read by the typing tab's Ctrl+wheel handler),
  and the per-effect-kind default-parameter editor (`crate::tabs::typing::EffectDefaultsEditorState`
  held on `SettingsTabState`, rendered via its `ui()`; a self-contained typing-panel widget that
  owns its own persistence to `TextTab.effect_defaults`, so settings needs no access to the private
  effect model).
- `ai_backend.rs`: AI backend pane UI for health display, process start/stop/restart/autostart,
  device/provider selection, max loaded models, and CUDA/ROCm diagnostics.
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
- To change projects directory or general user settings, edit `general.rs` and the matching
  persistence helper in `mod.rs`.
- To change text-typesetting options (hanging punctuation, Ctrl+wheel rotation mode), edit
  `typesetting.rs` and the matching persistence helper in `mod.rs`. Runtime globals for these
  live outside settings (`crate::text_punctuation`, `crate::tabs::typing::rotation_ctrl_wheel`).
- To change configurable shortcut UI or persistence, edit `hotkeys.rs` and coordinate with
  `src/input_manager_v2.rs`.
