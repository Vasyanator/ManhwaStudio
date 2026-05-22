/*
FILE HEADER (tools/mod.rs)
- Назначение: общий модуль переиспользуемых инструментальных примитивов UI/рисования,
  не привязанных к конкретной вкладке.
- Экспорт:
  - `MaskBrush`: переиспользуемая кисть для рисования бинарной маски в `egui::ColorImage`
    (радиус, hotkeys размера, Shift+wheel, отрисовка курсора, штрихи по сегменту).
  - `paint_line_with_brush`: общий helper штриха по `ColorImage` для круглой кисти.
*/

mod mask_brush;

pub use mask_brush::{MaskBrush, paint_line_with_brush};
