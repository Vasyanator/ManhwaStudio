# Module: crates/ms-config/src

## Purpose
The project's global configuration and runtime-path layer. It answers two questions for the whole application:

- **Where does anything live?** — `program_dir()` / `data_dir()` and every path helper
  derived from them (bundled resources, the Python env and backend, `user_config.json`,
  `ManhwaStudio_AI_Models`, `spell_check`, `last.log`, per-engine model trees).
- **What is a setting worth when nobody set it?** — `user_config_defaults()` /
  `project_config_defaults()`, and the `JsonConfig` load/merge/set wrapper that backfills
  a missing key without rewriting an already-complete document.

It is the hub every upper layer reads, so it depends on nothing above it: no egui, no
`tabs`, no `app`. GUI-free and cross-target.

## Architecture
```text
ms-log ─┐
ms-memory ─┤
ms-docstore ─┤
ms-text-util ─┼─> ms-config ──(re-exported as `crate::config`)──> manhwastudio_rs
ms-i18n ─┤
ms-thread ─┘
```

The binary re-exports this crate and several of its modules under binary-local names in
`src/main.rs` (other crates name `ms_config::` directly):

| shim | reaches |
|---|---|
| `pub use ms_config as config;` | `crate::config::…` |
| `pub use ms_config::app_tab;` | `crate::app_tab::…`, and `crate::tabs::AppTab` on top of it |
| `pub use ms_config::rotation_ctrl_wheel;` | `crate::rotation_ctrl_wheel::…`, and `crate::tabs::typing::rotation_ctrl_wheel::…` |
| `pub use ms_config::{config_saver, locale_store, version_format};` | `crate::config_saver::…` etc. (`locale_store` native-only) |

Every access to a config DOCUMENT goes through `ms-docstore`: `JsonConfig` (user config
and a title's `settings.json`), `load_user_config`, `load_raw_user_settings_for_startup`,
`update_user_config_file`, the section writers, `read_user_config_root` and the ORT
load-guard markers. The store reads through `ms_storage::global::storage()` on every
target (so the web build serves `user_config.json` from its in-memory/IndexedDB store),
writes natively by atomic temp+rename, and serializes every write per document.
`user_config_doc()` / `project_settings_doc(title_dir)` name the two documents for other
crates. Only non-document I/O (`ensure_model_dirs`, the locale folder) uses `std::fs`.

Two things that look like they belong further up live here anyway, and both for the same
reason: the default trees NAME them, and an inherent `impl` may only be written in the crate
that defines the type.

- `app_tab` — `General.enabled_tabs` is built from `AppTab::key()`.
- `bubble_status` — `Canvas.bubble_status_rules` embeds `default_bubble_status_rules_value()`.

## Files and submodules
- `lib.rs`: the crate root and the bulk of the layer — path roots and every derived path
  helper, `JsonConfig`, `update_user_config_file` (the serialized read-modify-write boundary
  for `user_config.json`), the two default trees, and the persisted enums that key paths:
  `AiInstallType`, `AiRuntime`, `Flux2Variant`, `MemoryProfile` re-reads, the `OrtLoadGuard`
  SIGILL decision model, and the interface-scale helpers. It also holds the TARGETED section
  writers of the settings surfaces (`save_advanced_form_search_params`, `save_text_language`,
  `save_hanging_punctuation`, `save_ai_runtime`, `save_onnx_provider_device`, `save_onnx_build`,
  `save_max_loaded_models`): each is ONE `update_user_config_root` transaction
  (`ms_docstore::update`) that edits ONE section, so every unrelated key survives and a
  malformed document is reported, never overwritten. They live here
  and not in the studio settings tab because their callers — `ms-settings-ui`'s panes and
  `ms-tab-typing` — may not depend on that tab. All of them do blocking I/O: never on the GUI
  thread.
- `version_format.rs`: pure, std-only composition (`compose_app_version`) and stripping
  (`version_core`) of the application version string. The ROOT `build.rs` pulls this exact file
  in with `include!("crates/ms-config/src/version_format.rs")`, so the code the build script
  runs is the code `cargo test` covers — which is why it must stay std-only: no `t!`, no
  logging, no reference to any other item of this crate.
- `app_tab.rs`: `AppTab` — the editor tab set. `ALL` + `key()` (the persistence contract) and
  the localized `title()` (display only). Edit when a tab is added or removed.
- `bubble_status.rs`: the GUI-free half of the bubble status rule model — kinds, fields,
  conditions, rules, the default preset, JSON conversion, normalization and evaluation.
  Border PAINTING is NOT here: it needs `egui::Painter` and lives in
  `crates/ms-widgets/src/bubble_status.rs`, which re-exports this module wholesale.
- `rotation_ctrl_wheel.rs`: the process-global Ctrl+wheel rotation mode of the typing tab
  (`Vector`/`Raster`) and its `DEFAULT_ROTATION_CTRL_WHEEL_MODE`, which the `TextTab` default
  tree reads. The module itself reads no config; the app seeds it at startup.
- `autosave_policy.rs`: the process-global autosave policy (`AutosavePolicy`: interval since the
  FIRST pending action + action threshold), with clamping setters and the startup seed. Same shape
  as `rotation_ctrl_wheel.rs`: it reads no config itself; `lib.rs` owns the `General` keys, bounds,
  defaults and the pure reader `autosave_policy_from_user_settings`. `ms_models::autosave_gate`
  reads it on every call, so a settings change applies live with no channel to the writers.
- `config_saver.rs`: the ONE debouncing, retrying writer thread every self-owned section of
  `user_config.json` is written through (`ms-widgets`' `PanelLayout` section and
  `ms-window-geometry`'s `Window` section). It sits in this crate because its write step IS `lib.rs`'s
  `update_user_config_file`; the feeders stay above. Owns the durability policy — 700 ms
  coalescing, a failed write HELD and retried with a capped backoff, a final attempt on
  shutdown, a definitive loss logged with cause/path/context.
- `settings_deep_link.rs`: `SettingsDeepLink` — the reveal targets one part of the app can ask the
  settings surface to open. It sits here because its REQUESTER (crate `ms-tab-typing`) and its
  CONSUMER (the binary's settings tab) may not depend on each other; `ms-settings-ui`'s
  `settings_shared.rs` re-exports it (`crate::settings_shared::SettingsDeepLink` in the binary).
- `ort_load_guard.rs`: the crash-safe `General.ort_load_state` markers written around every
  onnxruntime dylib load (`mark_ort_load_attempted` / `mark_ort_load_succeeded` /
  `reset_ort_load_guard`), plus the shared `read_user_config_root` the section writers in
  surfaces read for display. It sits here because its WRITER is crate `ms-native-runtime` and its READER
  (`read_ort_load_guard` / `ort_load_decision`) is `lib.rs`; the only surface offering the
  retry control is `ms-settings-ui`'s AI-backend pane, which calls `reset_ort_load_guard` here
  directly. Its writes use `Durability::ContentsAndDirectory` — an uncatchable SIGILL can
  arrive the instant the write returns.
- `storage_mode.rs`: the Dev/Prod document storage mode — `StorageMode` (frozen
  `"prod"`/`"dev"` under `General.storage_mode`, default Prod), `probe_storage_mode` (pure
  probe of a user-config document) and `init_storage_mode_at_startup`, which seeds
  `ms_docstore::set_default_format` as the process's FIRST document access (`main.rs`). Also
  the fonts-directory document locations (`app_fonts_dir`, `fonts_data_doc`,
  `fonts_presets_doc`); `app_fonts_dir` is the ONLY fonts-directory resolver
  (the typing tab owns those documents' contents but resolves the directory here). The conversion
  driver itself lives in `ms-project` (`storage_mode::convert_globals`).
- `locale_store.rs`: native-only (`#[cfg(not(target_arch = "wasm32"))]`) on-disk layer for the
  UI localization catalog. Unpacks the catalogs `ms-i18n` embeds into an editable
  `data_dir()/locale` folder, reconciles each file on every launch (never overwriting or
  deleting user values), and installs the active locale named by `General.ui_language`. It
  belongs to this layer because that tag comes out of `user_config.json`; on wasm there is no
  folder next to an executable and `web_entry.rs` installs the embedded catalog directly.

Two items in those files are TEST-ONLY and are gated `#[cfg(any(test, feature = "test-support"))]`
rather than `#[cfg(test)]`: `config_saver::test_harness` and `locale_store::GLOBAL_LOCALE_LOCK`.
A `#[cfg(test)]` item is invisible to a dependent crate, and both are used by the tests of crates
above this one (`ms-widgets`' panel-dock, `ms-window-geometry`, and every test in the
binary that installs a UI locale). Those crates enable `test-support` from their
`[dev-dependencies]`, so no production build carries either item.

## Contracts and invariants
- **No dependency may point upwards.** Nothing here may name `egui`, `tabs`, `app`, or any
  other item of the binary. A new need for one of those means the caller passes a value in,
  or the type moves down — not that this crate grows a dependency.
- **`user_config.json` transactions are serialized** by the `ms-docstore` document lock.
  Every mutation of that document is ONE `ms_docstore::update` — through
  `update_user_config_file` (or a section writer here); do not read-modify-write it
  anywhere else, and never write back a value read earlier outside the lock.
- **The document lock is the only lock of `user_config.json`, and it is NON-reentrant.**
  Inside an `update_user_config_file` mutator, or inside `ms_docstore::with_lock`/`update`
  of a user-config document, never call another user-config writer.
- **Default backfill has one rule, `merge_missing`** (insert absent keys, never replace
  values). Writers outside `JsonConfig` that must produce the same document shape (the
  installer's install-target config) call it instead of copying it.
- **A malformed document is never overwritten** by any writer here (`Malformed` →
  error, file untouched). A non-object root is still treated as `{}`.
- **`JsonConfig` backfills, it does not rewrite.** A semantically complete document is left
  byte-identical on disk; only genuinely missing keys are materialized.
  `set`/`set_path` re-read the document under the lock and edit only the named path.
  Its `DocKind` is inferred from the file name (`settings.json` → `ProjectSettings`, else
  `UserConfig`) unless `JsonConfig::with_kind` names it.
- **Persisted spellings are frozen.** `AppTab::key()`, `Flux2Variant::wire()` /
  `dir_name()` / `settings_file_name()`, `MemoryProfile::as_config_str()`,
  `StorageMode::as_config_str()` (A11) and the
  `bubble_status` field names are on disk in users' documents. They are byte-stable across
  releases AND across UI languages, and are never localized
  (`dev-docs/i18n_exclusions.md` A3/A5/B1). Localized labels (`title()`, `label()`,
  `summary()`) are display-only and must never reach a file or the wire.
- **The storage mode is seeded before any document access.** The startup probe reads the mode
  from whichever file user_config is stored in; a missing key means Prod; a user_config in
  the other format than its recorded mode means a conversion is pending (the driver converts
  user_config LAST, so it is the sentinel). A malformed user_config selects the mode of its
  existing file and schedules nothing.
- **Config document I/O goes through `ms-docstore`.** A direct `std::fs` (or storage-seam)
  read or write of `user_config.json` / `settings.json` here would bypass the document
  lock and the atomic write, and break the web build.
- **Standalone clippy on BOTH targets is part of verification.** A workspace-wide run unifies
  features and can mask a missing one; run
  `cargo clippy -p ms-config --target x86_64-unknown-linux-gnu -- -D warnings` and the same
  for `x86_64-pc-windows-gnu` (the rule that caught `ms-sysprobe`'s missing
  `windows-sys/Win32_Security`).

## Editing map
- A new path root, model directory or config file name: `lib.rs`, next to its neighbours.
- A new user or project setting's default: the corresponding tree in `lib.rs`. Remember that
  `merge_missing` adds but never removes — a renamed key leaves the old one on disk.
- Autosave interval/threshold bounds or defaults: the `AUTOSAVE_*` consts in `lib.rs`; the
  runtime global in `autosave_policy.rs`; the flush decision itself in `ms_models::autosave_gate`.
- A new editor tab: `app_tab.rs` (`ALL`, `key()`, `title()`), then the `enabled_tabs` default
  in `lib.rs` and the tab's own module in `src/tabs/`.
- A new bubble status condition or border kind: `bubble_status.rs` for the model, and
  `crates/ms-widgets/src/bubble_status.rs` if it also needs to be drawn.
- A new FLUX.2 variant fact: the single `impl Flux2Variant` in `lib.rs` if it keys a PATH, a
  persisted document, an id or a capability gate — one block, so a new difference cannot drift
  across two mapping tables. If it is PRESENTATION (a caption, a repository id), it belongs to
  the `Flux2VariantPresentation` extension trait in
  `crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux2_klein/variant_presentation.rs`
  instead: this crate is below the UI and the engine is the only consumer.
