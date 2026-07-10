# Module: src/tabs/settings

## Purpose
Settings tab implementation for user and project options that affect the editor runtime: general
launcher settings, global memory profile, shared canvas/ribbon behavior, AI backend process/device
controls, and hotkey overrides.

## Architecture
`SettingsTabState` in `mod.rs` owns the active-section id, shared settings snapshots, the
hot-applicable `MemoryManager` binding, and bindings to shared models. Section identity, the
studio section list, and the tab-bar order come from the cross-surface registry
`crate::settings_shared` (`SettingsSectionId`, `sections_for(Studio)`, `title_key(id, Studio)`),
so the launcher and studio share one source of truth for section metadata.

The three cross-surface "double-interface" sections (General / AiBackend / Tutorials) are owned by
a single `crate::settings_shared::SharedSettingsPanels` embedded on `SettingsTabState` and rendered
via `shared.draw(id, ui, Studio, &ai_backend_handle)`; the studio-only sections
(CanvasRibbon / Typesetting / Hotkeys) render through this module's own methods. The AI backend
section no longer owns the backend process: `SettingsTabState` holds a cloned `AiBackendHandle`,
passed by reference into `shared.draw`. Pane files render focused UI sections through methods on
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
- `mod.rs`: `SettingsTabState`, section routing (over `settings_shared::sections_for(Studio)`),
  canvas settings binding, the shared `AiBackendHandle`, the `SharedSettingsPanels` container for
  the General / AiBackend / Tutorials sections, typing-layout persistence helper, and the coalesced
  canvas settings save worker. The AiBackend and Tutorials sections are rendered inline in `draw` via
  `shared.draw(...)` (AiBackend inside the scroll area); the General section goes through `general.rs`.
  (The projects-dir + memory-profile editing/persistence moved to the shared
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
- `general.rs`: thin studio wrapper for the general section. Enforces the studio-only vertical
  typing-panel layout (persisted off-thread via `save_typing_panel_layout`), then renders the shared
  General section through `SettingsTabState.shared.draw(General, ..)` (projects-directory editor,
  global memory-profile combo, UI-language selector, typesetting-language selector) and applies the
  memory-profile runtime effect from its outcome. Projects-dir + memory-profile + UI-language
  persistence live inside the shared `crate::general_settings_panel` widget, not here. The
  UI-language selector (a `WheelComboBox`) lists the locales found in the on-disk
  `locale/` folder — scanned ONCE at widget construction, never per frame — each shown by its
  `_meta.name`; changing it persists `General.ui_language` (synchronously, like the memory-profile
  write) and live-installs that locale's catalog via `crate::locale_store::install_ui_locale`
  (no restart). The shared widget also renders a SECOND surface for the typesetting-language
  selector that `typesetting.rs` owns (so both languages are chosen side by side, and the setting is
  reachable from the launcher). Both surfaces read/write the `ms_text_util::language` process-global
  and persist through the same `save_text_language`, so they cannot drift apart.
- `canvas_ribbon.rs`: shared ribbon/canvas pane for bubble type defaults, aside/on-top layout,
  spellcheck word lists, bubble status rules, and related `SharedCanvasSettings` fields.
- `typesetting.rs`: "Тайп" pane for text-typesetting options: the app-wide
  hanging-punctuation list editor (`TextTab.hanging_punctuation`, applied live via
  `crate::text_punctuation` and persisted through `save_hanging_punctuation` in `mod.rs`)
  the "Поворот Ctrl+колесо" chooser (`TextTab.rotation_ctrl_wheel_mode`, applied live
  via the `crate::tabs::typing::rotation_ctrl_wheel` global and persisted through
  `save_rotation_ctrl_wheel_mode` in `mod.rs`; read by the typing tab's Ctrl+wheel handler),
  the typesetting-language selector — the SHARED
  `crate::general_settings_panel::draw_text_language_setting(ui, id_salt)` function (two
  `WheelComboBox`es: `ScriptGroup` then the concrete `TextLanguage` within it; changing the group
  selects that group's first language), the SAME selector the general-settings widget renders. It
  applies live via `ms_text_util::language::set_text_language` (the typing tab's `panel/facade.rs`
  observes `text_language()` each frame and re-runs font-coverage classification off-thread) and
  persists `TextTab.text_language` (the `lang.tag()`) through `save_text_language` in `mod.rs` on a
  background thread; the process-global atomic is the single source of truth, so no selection state is
  stored on `SettingsTabState`. `typesetting.rs` passes the id-salt prefix
  `"settings.typesetting.text_language"` so its egui ids stay distinct from the general widget's,
  the per-effect-kind default-parameter editor (`crate::tabs::typing::EffectDefaultsEditorState`
  held on `SettingsTabState`, rendered via its `ui()`; a self-contained typing-panel widget that
  owns its own persistence to `TextTab.effect_defaults`, so settings needs no access to the private
  effect model), and the "Настройки шрифтов" block
  (`crate::tabs::typing::FontSettingsEditorState`, same double-interface pattern): it lists the app's
  fonts in three categories rendered in their own typefaces and imports/removes system fonts via the
  runtime-global imported-fonts store, loading font lists off the GUI thread. Both blocks are wrapped
  in collapsed `CollapsingHeader`s.
- `hotkeys.rs`: configurable hotkey list, live shortcut capture, reset/clear actions, and
  `user_config.json` override persistence.

The AI backend and tutorials sections have no studio-local file: they are rendered inline in
`mod.rs::draw` through `SharedSettingsPanels::draw`, which forwards to the shared
`crate::ai_backend_panel::draw_ai_backend_panel` and `crate::tutorial::draw_tutorials_pane`. The
`SharedSettingsPanels` container (in `crate::settings_shared`) owns the panel scratch state and the
tutorial progress handle; the studio passes its `AiBackendHandle` in by reference. The AI backend
panel hosts the "ONNX-инференс" runtime selector, the OFFLINE-capable RUNTIME-BRANCHED ONNX
selection (Native build/EP/device combos vs Backend unified provider/device combos), and the shared
model-limit slider; its persistence writers (`save_ai_runtime` / `save_onnx_build` /
`save_onnx_provider_device` / `save_max_loaded_models`) and the ORT load-guard writers live in
`mod.rs`.

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
- UI-visible strings are localized through the `ms-i18n` `t!` / `tf!` macros (crate-wide via
  `#[macro_use] extern crate ms_i18n;` in `main.rs`); the Russian source lives in
  `crates/ms-i18n/locales/{ru,en}.json`. Enum wire tokens (`AsideBubbleCompactMode::as_str`, …),
  serde field names, and `user_config.json` keys stay byte-identical English/serde and must NOT be
  routed through `t!` (see `docs/i18n_exclusions.md`). Any `WheelComboBox::from_label` /
  `egui::CollapsingHeader::new` / `ui.collapsing` whose label is localized MUST carry a stable
  `.id_salt(...)` (or be built as `CollapsingHeader::new(...).id_salt(...).show(...)`) so the widget
  id does not follow the translated text (`docs/i18n_exclusions.md` §C).

## Editing map
- To add or rename a settings section, edit the registry `crate::settings_shared`
  (`SettingsSectionId`, `SECTIONS`, `title_key`), then the dispatch `match` in `mod.rs::draw`
  (exhaustive, no `_ =>`), and add a focused renderer if it grows beyond trivial UI. The tab bar and
  order come from `sections_for(Studio)`; do not hand-list sections in `mod.rs`.
- To change shared canvas/ribbon settings, edit `canvas_ribbon.rs` and the save/apply helpers in
  `mod.rs`.
- To change AI backend process controls, process logs, autostart, or device/probe commands, edit the
  shared `crate::ai_backend_panel` widget and the `save_*` worker functions in `mod.rs`; the studio
  renders it inline in `mod.rs::draw` (scroll area + `shared.draw(AiBackend, ..)`).
- To change the projects directory, the memory profile, or the UI-interface language, edit the shared
  `crate::general_settings_panel` widget (used by both the studio pane and the launcher); the studio
  `general.rs` is only a thin wrapper that adds the typing-panel-layout enforcement and applies the
  memory-profile runtime effect. The UI-language list is scanned from `locale/` once at construction;
  installation goes through `crate::locale_store::install_ui_locale`.
- To change text-typesetting options (hanging punctuation, Ctrl+wheel rotation mode, typesetting
  language), edit `typesetting.rs` and the matching persistence helper in `mod.rs`
  (`save_text_language` for the language). Runtime globals for these live outside settings
  (`crate::text_punctuation`, `crate::tabs::typing::rotation_ctrl_wheel`,
  `ms_text_util::language`). The typesetting-language selector is duplicated in
  `crate::general_settings_panel`; a change to its behavior must be mirrored there (both share the
  catalog keys, the process-global, and `save_text_language` — only the egui `id_salt`s differ).
- To change the collapsed effect-defaults / font-settings blocks in the "Тайп" pane, edit their
  widgets in `src/tabs/typing/panel/` (`effect_defaults.rs` / `font_settings.rs`); `typesetting.rs`
  only wraps each `ui()` in a `CollapsingHeader`.
- To change configurable shortcut UI or persistence, edit `hotkeys.rs` and coordinate with
  `src/input_manager_v2.rs`.
