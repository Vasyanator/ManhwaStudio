/*
File: tabs/mod.rs

Purpose:
The tab shim map. Every project-editor tab except `settings/` now lives in its own crate;
this file re-exports each one under the module name its call sites already use, so no
`crate::tabs::<tab>::…` path in the binary had to change. It also re-exports the shared
`AppTab` selector enum.

Key structures:
- AppTab: declared in `ms_config::app_tab` (with `ALL`, the persistence `key()` and the
  localized `title()`) and re-exported here, so `crate::tabs::AppTab` stays the path
  everything uses.

Notes:
`key()` and `title()` are deliberately split (see `docs/i18n_exclusions.md` B1):
`key()` is the byte-stable English identifier used for persistence and must never
change with the UI language, while `title()` is a localized label for display only.
Both live next to the enum: an inherent `impl` may only be written in the crate that
defines the type, and the enum sits in `ms-config` because the `General.enabled_tabs`
default tree is built from `key()`.
*/

pub mod settings;
// The «Текст» tab now lives in the standalone `ms-tab-typing` crate — 80 000 lines, the
// largest tab of the project, and the whole of it (state, canvas hooks, panel UI, text
// store, PSD export). The re-export keeps every `crate::tabs::typing::…` path valid in
// `app.rs`, the settings tab and the PS editor without touching a call site. It sits at
// the TOP of the library stack: nothing below it may name it back.
pub use ms_tab_typing as typing;
// The four small tabs («Персонажи», «Термины», «Заметки», «Вики») now live in the
// standalone `ms-tabs-simple` crate. They share one crate because their dependency sets
// are identical and four separate crates would only deepen the build graph. The
// re-export keeps every `crate::tabs::characters::…` / `::terms::…` / `::notes::…` /
// `::wiki::…` path valid in `app.rs`, `launcher/app.rs` and the translation tab without
// touching a call site.
pub use ms_tabs_simple::{characters, notes, terms, wiki};
// The «Перевод» tab now lives in the standalone `ms-tab-translation` crate — tab state,
// the OCR / detector / machine-translation controllers and workers, the concrete MT
// backends and every subpanel. The re-export keeps every `crate::tabs::translation::…`
// path valid in `app.rs`, the AI backend panel/supervisor, the launcher's reline flow and
// the `cleaning` tab without touching a call site.
pub use ms_tab_translation as translation;
// The «PS-подобный редактор» tab now lives in the standalone `ms-tab-ps-editor` crate —
// viewport, layer stack, tiled texture cache, the whole tool set and the view-only
// «Коррекция» GL pass. The re-export keeps every `crate::tabs::ps_editor::…` path valid in
// `app.rs` and the `page_manager` tab without touching a call site. It owns no GL objects:
// the «Коррекция» shader belongs to the `egui-shader-layers` backend, which
// `studio_bootstrap` installs at window creation and destroys in its `on_exit`.
pub use ms_tab_ps_editor as ps_editor;
// The «Менеджер страниц» tab now lives in the standalone `ms-tab-page-manager` crate —
// the page card grid and every structural dialog (crop / split / stitch / clean). The
// re-export keeps every `crate::tabs::page_manager::…` path valid in `app.rs` without
// touching a call site. The tab still only REQUESTS structural page ops; `app.rs`
// executes them.
pub use ms_tab_page_manager as page_manager;
// The «Клининг» tab now lives in the standalone `ms-tab-cleaning` crate — the largest tab
// of the project and the whole of it (tab state, the complete tool set with every AI
// editor engine, the autoclean driver and the chapter watermark engine). The re-export
// keeps every `crate::tabs::cleaning::…` path valid in `app.rs` without touching a call
// site. It sits at the TOP of the library stack, above typing and translation.
pub use ms_tab_cleaning as cleaning;

pub use crate::app_tab::AppTab;
