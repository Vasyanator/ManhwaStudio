/*
FILE HEADER (tools/mod.rs)
- Назначение: общий модуль переиспользуемых инструментальных примитивов UI/рисования,
  не привязанных к конкретной вкладке.
- Экспорт:
  - `MaskBrush`: переиспользуемая кисть для рисования бинарной маски в `egui::ColorImage`
    (радиус, hotkeys размера, Shift+wheel, отрисовка курсора, штрихи по сегменту).
  - `fill_polygon_spans`: even-odd сканлайн-растеризация замкнутого полигона в горизонтальные
    спаны (общая для лассо PS-редактора и инструментов клининга).
*/

mod mask_brush;
mod polygon_mask;

pub use mask_brush::MaskBrush;
pub use polygon_mask::fill_polygon_spans;
