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
`ms-memory` / `ms-onnx`, and above exactly ONE tab crate — `ms-tab-translation`, for
`backend_health` (the health snapshot and probe-command model). The tabs never depend back
on this crate; if a reference in that direction appears, the layering is wrong, not the
dependency list.

## Files and submodules
- `lib.rs`: crate root and module list; the `tutorial` feature gate.
- `settings_shared.rs`: the menu-level layer — `SettingsSectionId`, `sections_for`,
  `title_key`, and `SharedSettingsPanels`, which owns the three shared panes.
- `general_settings_panel.rs`: projects root, memory profile, UI language, UI scale,
  typesetting language, startup monitor. Also owns the process-wide UI-scale slot
  (`ui_scale_percent` / `apply_ui_scale`) every `eframe::run_native` creator applies, and
  the locale-catalog helpers the launcher's first-run language modal reuses.
- `ai_backend_panel.rs`: AI runtime selection, ONNX provider/device/build, model limit,
  backend health readout, ORT crash-guard reset.
- `ai_backend_supervisor.rs`: `AiBackendHandle` — the app-global handle both shells drive
  the Python backend process through, plus its health probe and (on Windows) the loopback
  WebSocket handshake.
- `tutorial/`: the onboarding subsystem, behind the `tutorial` feature. See its own
  `MODULE_README.md`.

## Contracts and invariants
- **GUI thread.** No pane probes, spawns, downloads or writes config on the GUI thread:
  every such step goes to a named `ms_thread` worker and reports back over a channel.
- **Config writes** go through the `ms_config` section writers (`save_ai_runtime`,
  `save_onnx_provider_device`, `save_onnx_build`, `save_max_loaded_models`,
  `save_text_language`, `persist_config_keys`), never by rewriting `user_config.json` here.
  That is also why those writers live in `ms-config` and not in the studio settings tab.
- **The `tutorial` feature is NOT inherited.** The root crate forwards it
  (`tutorial = ["ms-settings-ui/tutorial"]`). Enabling one without the other silently drops
  the tours from half the app.
- `tutorial/engine.rs` is ALSO mounted by the `tutorial_test` demo bin through `#[path]`,
  so it must stay dependency-light: egui + std only, no config, no logging, no `t!`.
- No literal user-visible strings: `t!` / `tf!` / `tp!` only, and a stable `id_salt` on any
  localized label.
- Native-only pieces (ORT runtime, monitor list, backend subprocess) keep their
  `cfg(not(target_arch = "wasm32"))` gates INSIDE the files; the matching crates are
  declared in the target table of `Cargo.toml`.

## Editing map
- To add a settings section, see `settings_shared.rs` (`SettingsSectionId`, `sections_for`,
  `title_key`) and add both halves of the double interface in the new pane.
- To change what the AI pane offers or reports, see `ai_backend_panel.rs`; to change how the
  backend process is started, watched or stopped, see `ai_backend_supervisor.rs`.
- To change a persisted settings key, add the writer in `ms-config` first, then call it here.
- To change the tours, see `tutorial/` (`id.rs` for the stable keys, the per-surface step
  scripts live with their surfaces in the launcher).
