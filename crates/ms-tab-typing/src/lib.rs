/*
FILE HEADER (crates/ms-tab-typing/src/lib.rs)
- Назначение: crate root вкладки `Текст`. Реэкспортируется бинарником из
  `src/tabs/mod.rs` как `crate::tabs::typing`, поэтому все существующие пути
  `crate::tabs::typing::…` продолжают работать.
- Слой: вершина библиотечного стека — выше canvas/models/widgets и ниже только
  `app.rs`, который зовёт `TypingTabState::draw`. Крейт не имеет права называть
  `app`, `launcher` или другую вкладку.
- Содержимое:
  - `auto_typing`: алгоритм авто-тайпа (оптический центр оверлея + поиск пузыря по
    composited-странице `src + clean overlay` из shared cache).
  - `tab`: основное состояние вкладки и логика работы с `CanvasView`/оверлеями.
  - `render_next`: текущий продовый путь рендера с публичным контрактом в `types.rs`
    и реализацией в `pipeline.rs`.
  - `panel`: верхняя фиксированная панель вкладки `Текст` (layout + режимы).
  - `mask`: бинарная маска обрезки страниц (загрузка/редактирование/сохранение/клип).
  - `segmentation`: сегментатор текста (разбивка на блоки + правила соединения при
    переносе) с языко-нейтральным `base` и реализациями языков (`ru`).
  - `rotation_ctrl_wheel`: app-wide runtime-global выбор режима поворота Ctrl+колесо
    (Vector/Raster); пишется из Settings «Тайп», читается в Ctrl+wheel-хендлере.
    Сам модуль живёт в крейте `ms-config` (`crates/ms-config/src/rotation_ctrl_wheel.rs`) и здесь реэкспортируется.
*/

#![warn(clippy::all)]

// The `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`), mounted crate-wide exactly as
// `src/main.rs` mounts them for the binary: this tab's localized labels use the bare
// macro names the extraction tool emits and the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

mod auto_typing;
// The ONLY sanctioned entry point for non-typing code into font administration
// (loaders, imported-fonts store, display-name overrides, the `FontEntry` type). The
// settings font-settings UI (`src/tabs/settings/typesetting/`) imports this and nothing
// else from typing. See `font_admin.rs` for the contract.
pub mod font_admin;
mod mask;
mod panel;
mod psd_export;
// Pure page-re-pagination engine: stitches composed export pages into same-width ribbons
// and re-slices them into pages of a chosen aspect ratio / fixed height. GUI-free, no I/O.
mod export_repaginate;
// Multi-page PDF writer used by the `Pdf` export format: one full-page raster per page.
mod pdf_export;
// The Ctrl+wheel rotation-mode global now lives in `ms-config` (re-exported by `main.rs` as
// `ms_config::rotation_ctrl_wheel`) so that the config default tree can read its default without
// depending on `tabs`. This re-export keeps existing
// `ms_config::rotation_ctrl_wheel::…` paths valid.
pub use ms_config::rotation_ctrl_wheel;
// The text renderer now lives in the `ms-text-render` crate. Re-export keeps
// existing `crate::render_next::…` paths valid across the binary.
pub use ms_text_render as render_next;
// `segmentation` moved to the `ms-text-util` crate. Re-export keeps existing
// `crate::segmentation::…` paths valid.
pub use ms_text_util::segmentation;
mod tab;

pub use panel::{TypingPanelLayout, TypingTopPanelState};
// Editor widget for per-effect-kind default parameters, rendered by the settings pane.
pub use panel::EffectDefaultsEditorState;
// Startup seeding of the runtime-global effect-defaults store from user config.
pub use panel::seed_effect_defaults_from_config;
// Startup seeding of the runtime-global imported-system-fonts store from user config.
pub use panel::seed_imported_system_fonts_from_config;
// The advanced text-form search knobs (`TextTab.advanced_form_search`). Re-exported
// because a config-owning site lives outside typing: the startup seed
// (`main.rs::seed_advanced_form_search_from_config`). The knob object's PLACEMENT in
// `user_config.json` is `ms_config::save_advanced_form_search_params`; its SHAPE stays
// here, in `advanced_form_params`.
pub use panel::advanced_form_params;
pub use tab::TypingTabState;
// Per-frame inputs of `TypingTabState::draw`, built by `app.rs` — it carries the
// app-owned `PanelDockState` borrow the tab draws its panels from.
pub use tab::TypingDrawParams;
// This tab's default panel arrangement, handed to the app-owned dock state as a
// `fn` pointer (restore before the first frame + `ensure_default_layout` per frame).
pub use tab::typing_default_dock_layout;
// Reason tag for `TypingTabState::flush_text_layers_if_dirty`; `app.rs` names it at the tab-leave and
// exit flush points.
pub use tab::TypingSaveFlushReason;
// Failure of `TypingTabState::flush_text_layers` — a flush that could not run at all, as opposed to
// one that ran and owned no pages. `app.rs` names it: its page-op quiesce gate treats an unwired text
// store (the Text tab was never opened) differently from a poisoned document lock.
// `tab::TypingTextFlushOutcome` stays unexported: callers match the `Ok` value without naming it, and
// an unused re-export is a warning.
pub use tab::TypingTextFlushError;
// Re-export the shared text-preview helper so other tabs (PS editor) reuse the same logic.
pub use tab::text_preview_label;
