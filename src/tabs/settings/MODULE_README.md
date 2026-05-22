# Module: src/tabs/settings

## Purpose
Settings tab implementation for user and project options that affect the editor runtime: general
launcher settings, global memory profile, shared canvas/ribbon behavior, AI backend process/device
controls, and hotkey overrides.

## Architecture
`SettingsTabState` in `mod.rs` owns pane selection, shared settings snapshots, AI backend process
runtime state, the hot-applicable `MemoryManager` binding, and bindings to shared models. Pane files
render focused UI sections through methods on `SettingsTabState`.

Settings writes are offloaded to worker threads or a coalescing save worker. The GUI thread updates
the in-memory `SharedCanvasSettings`, sends save requests, polls shared snapshots, and forwards
commands to the AI backend process/probe workers.

AI backend health probing is shared with the Translation tab through
`tabs::translation::backend_health`. Process management for `ai_backend.py` is local to this module
and uses `python_manager`/`config` for runtime paths instead of constructing Python paths directly.
Before launch, the process manager checks the default backend port and, if it is occupied, selects
a free localhost port, publishes it through `backend_health`, and passes it to Python with `--port`.

## Files and submodules
- `mod.rs`: `SettingsTabState`, pane routing, canvas settings binding, AI backend process worker,
  backend stdout/stderr readers, autostart/projects-dir/typing-layout persistence helpers, and
  coalesced canvas settings save worker.
- `general.rs`: general pane UI, including global memory profile, projects directory, and the
  current typing panel layout persistence behavior.
- `canvas_ribbon.rs`: shared ribbon/canvas pane for bubble type defaults, aside/on-top layout,
  spellcheck word lists, bubble status rules, and related `SharedCanvasSettings` fields.
- `ai_backend.rs`: AI backend pane UI for health display, process start/stop/restart/autostart,
  device/provider selection, max loaded models, and CUDA/ROCm diagnostics.
- `hotkeys.rs`: configurable hotkey list, live shortcut capture, reset/clear actions, and
  `user_config.json` override persistence.

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
- To change configurable shortcut UI or persistence, edit `hotkeys.rs` and coordinate with
  `src/input_manager_v2.rs`.
