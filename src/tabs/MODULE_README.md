# Module: src/tabs

## Purpose
Tab wiring layer for project-scoped workflows. Every tab except `settings/` now lives in its
own crate; this directory owns only the re-export shims that keep their call-site paths valid,
plus the settings tab itself.

## Architecture
`mod.rs` is a shim map. Each extracted tab is re-exported under the module name its call sites
already use, so no `crate::tabs::<tab>::…` path anywhere in the binary had to change:

| path | crate |
|---|---|
| `tabs::typing` | `ms-tab-typing` |
| `tabs::characters` / `terms` / `notes` / `wiki` | `ms-tabs-simple` |
| `tabs::translation` | `ms-tab-translation` |
| `tabs::ps_editor` | `ms-tab-ps-editor` |
| `tabs::page_manager` | `ms-tab-page-manager` |
| `tabs::cleaning` | `ms-tab-cleaning` |

`settings/` stays a module of the binary: it reads `general_settings_panel`, `ai_backend_panel`,
`settings_shared`, `input_manager_v2` and `tutorial`, several of which are binary-only.

The crates form a strict downward chain — `ms-tabs-simple` -> `ms-tab-translation`,
`ms-tab-typing` -> `ms-tab-ps-editor` -> `ms-tab-page-manager`, and `ms-tab-cleaning` on top of
typing and translation. No tab crate may name `app`, `launcher` or a tab above it; `app.rs`
calling `TabState::draw` is the only direction across the boundary.

`mod.rs` also re-exports the `AppTab` enum used by the root app and hotkey scopes; the enum
itself (with `ALL`, `key()` and the localized `title()`) is declared in
`crates/ms-config/src/app_tab.rs` so that `config.rs` can read the persistence ids without
depending on `tabs`.

The shared canvas engine remains in `crates/ms-canvas/src/`. Translation, cleaning, and typing customize it
through `CanvasHooks` or typed canvas APIs. Shared persistent state lives in `crates/ms-models/src/` and
`ProjectData`; tab code should snapshot shared models before expensive work and release locks
before rendering, file I/O, image processing, AI requests, or worker waits.

Long-running work is worker-driven. Tab `draw` methods may render state, poll channels, and upload
already prepared textures, but must not block the GUI thread with project scans, image decode,
model/backend calls, text rendering, export, or synchronous save-heavy workflows.

## Files and submodules
- `mod.rs`: the re-export shims listed above, plus the `AppTab` re-export
  (`pub use crate::app_tab::AppTab`). `AppTab::key()` (stable English persistence id, byte-stable
  across releases and UI languages; the `General.enabled_tabs` config keys) and `AppTab::title()`
  (localized display label) are kept deliberately distinct and both live with the enum in
  `crates/ms-config/src/app_tab.rs`. See `docs/i18n_exclusions.md` §B1.
- `settings/`: project/user settings panes for general options, shared ribbon/canvas settings,
  AI backend process/device controls, and hotkey overrides. The only tab still compiled into the
  binary, because it reads binary-only modules (`general_settings_panel`, `ai_backend_panel`,
  `settings_shared`, `tutorial`).

The extracted tabs document themselves in their own crates:
`crates/ms-tabs-simple/src/`, `crates/ms-tab-translation/src/MODULE_README.md`,
`crates/ms-tab-typing/src/MODULE_README.md`, `crates/ms-tab-ps-editor/src/MODULE_README.md`,
`crates/ms-tab-page-manager/src/MODULE_README.md`, `crates/ms-tab-cleaning/src/MODULE_README.md`.

## Contracts and invariants
- `AppTab::ALL`, `AppTab::key`, `AppTab::title`, and root app tab routing must stay in sync when
  adding or removing tabs. `key()` is the persistence contract (used by `config.rs` to build the
  `General.enabled_tabs` defaults) and must never be localized; `title()` is display-only.
- Canvas behavior shared across tabs belongs in `crates/ms-canvas/src/`; tab-specific behavior should use
  hooks or narrow typed APIs instead of duplicating canvas state machines.
- Durable chapter data must flow through `ProjectData`, shared models, or explicit project path
  contracts. Do not hard-code project-relative paths that already exist in `ProjectPaths`.
- GUI-thread tab code must not perform blocking file I/O, image decode, AI/network requests,
  large parsing, export, or worker joins.
- Shared model locks must be short-lived and released before callbacks, canvas rendering, disk
  writes, or expensive computation.
- Every FLOATING panel of a tab (a surface hovering over the canvas with a title strip, not an
  edge-glued `egui::Panel`) must be a TAB OF THE PANEL DOCK: `CollapsiblePanel` + `PanelTab` from
  `widgets/panel_dock/`, declared through `PanelDock::begin` → `.tab(id)` → `.end(&mut cx)`. A
  hand-rolled `Area + Frame::popup` panel or an `egui::Window` used as a panel is a defect — it
  loses docking, collapse, persistence, and tear-off into an OS window. `ms-tab-typing`'s `tab.rs` is the
  reference call site; `egui-docs/01-app-shell.md` §3.1 is the recipe. The floating surfaces of
  `ms-tab-cleaning`, `ms-tab-translation`, and `ms-tab-ps-editor` are not migrated yet — that is
  phased debt, not an exemption for new panels. `Area` stays legitimate for toasts, tooltips, and scene-anchored
  overlays.
- New maintained source subdirectories under `tabs/` need their own `MODULE_README.md`.
- A tab crate may never name `app`, `launcher` or a tab crate above it in the chain. Anything a
  tab needs from the binary must move DOWN into a library crate first (that is how
  `input_manager_v2` came to live in `ms-widgets`).
- An item a tab crate exposes to the binary or to a tab above it must be `pub`, not `pub(crate)`,
  and must carry a comment naming the outside caller.

## Editing map
- To add a new top-level tab, create its crate, add the shim to `mod.rs`, and update root
  `MangaApp` tab construction/routing, hotkey scopes if needed, and this document.
- To change OCR, detection, MT, or translation footer behavior, edit `crates/ms-tab-translation/src/`.
- To change clean-overlay tools or quick-clean behavior, edit `crates/ms-tab-cleaning/src/`.
- To change text overlays, renderer integration, masks, or export, edit `crates/ms-tab-typing/src/`.
- To change the layered single-page editor, edit `crates/ms-tab-ps-editor/src/`.
- To change the page grid or structural page dialogs, edit `crates/ms-tab-page-manager/src/`.
- To change character/term/notes/wiki workflows, edit `crates/ms-tabs-simple/src/`.
- To change shared ribbon/canvas settings, AI backend controls, or hotkey UI, edit `settings/`.
