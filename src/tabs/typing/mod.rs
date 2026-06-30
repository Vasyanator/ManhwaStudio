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
*/
mod auto_typing;
mod mask;
mod panel;
mod psd_export;
pub mod render_next;
mod segmentation;
mod tab;

pub use panel::{TypingPanelLayout, TypingTopPanelState};
pub use tab::TypingTabState;
// Re-export the shared text-preview helper so other tabs (PS editor) reuse the same logic.
pub(crate) use tab::text_preview_label;
