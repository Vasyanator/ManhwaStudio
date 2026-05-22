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
*/
mod auto_typing;
mod mask;
mod panel;
pub mod render_next;
mod tab;

pub use panel::{TypingPanelLayout, TypingTopPanelState};
pub use tab::TypingTabState;
