/*
FILE HEADER (tabs/typing/mod.rs)
- Назначение: модуль вкладки `Текст`.
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
*/
mod auto_typing;
mod mask;
mod panel;
mod psd_export;
// Typing-only Ctrl+wheel rotation-mode global. `pub` so the settings "Тайп" pane can
// read/write it via `crate::tabs::typing::rotation_ctrl_wheel::…`.
pub mod rotation_ctrl_wheel;
// The text renderer now lives in the `ms-text-render` crate. Re-export keeps
// existing `crate::tabs::typing::render_next::…` paths valid across the binary.
pub use ms_text_render as render_next;
// `segmentation` moved to the `ms-text-util` crate. Re-export keeps existing
// `crate::tabs::typing::segmentation::…` paths valid.
pub use ms_text_util::segmentation;
mod tab;

pub use panel::{TypingPanelLayout, TypingTopPanelState};
// Editor widget for per-effect-kind default parameters, rendered by the settings pane.
pub(crate) use panel::EffectDefaultsEditorState;
// Startup seeding of the runtime-global effect-defaults store from user config.
pub(crate) use panel::seed_effect_defaults_from_config;
pub use tab::TypingTabState;
// Re-export the shared text-preview helper so other tabs (PS editor) reuse the same logic.
pub(crate) use tab::text_preview_label;
