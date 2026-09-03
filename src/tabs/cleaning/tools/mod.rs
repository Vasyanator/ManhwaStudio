/*
FILE HEADER (tabs/cleaning/tools/mod.rs)
- Назначение: корневой модуль инструментов клининга.
- Экспорт:
  - `CleaningTool`, `StrokePoint`, `StrokeModifiers` из `base.rs`.
  - Конкретные инструменты вкладки cleaning:
    `ZamazkaTool`, `StampTool`, `GradientFillTool`, `TextureSynthesisInpaintTool`,
    `LamaInpaintTool`, `LamaMpeInpaintTool`, `AotInpaintTool`, `SdxlInpaintTool`,
    `FluxFillInpaintTool`, `WatermarkRemovalTool`, `Flux2KleinTool`,
    `AiEditorTool`.
- Внутренние модули без экспорта:
  - `watermark_library` — библиотека измеренных знаков на диске; используется
    режимом «По главе» из `watermark_removal.rs`.
  - `watermark_entry` — мост между движком разложения и библиотекой: приём
    эталонных кадров, отображение вердиктов и подбор записей по подписи знака.
  - `watermark_library_window` — окно управления библиотекой, открываемое из
    инструмента.
  - `region_edit_v2` — the on-canvas region-editing framework (`RegionFrame`, its mask
    layers and its geometry). Consumed by `ai_editor`; see its own `MODULE_README.md`.
*/
mod base;

pub use base::StrokeModifiers;
pub use base::{CleaningCursorOccluder, CleaningTool, StrokePoint};

mod gradient;
pub use gradient::GradientFillTool;

mod texture_synthesis;
pub use texture_synthesis::TextureSynthesisInpaintTool;

mod lama;
pub use lama::LamaInpaintTool;

mod sdxl;
pub use sdxl::SdxlInpaintTool;

mod flux_fill;
pub use flux_fill::FluxFillInpaintTool;

mod lama_mpe;
pub use lama_mpe::LamaMpeInpaintTool;

mod aot;
pub use aot::AotInpaintTool;

mod zamazka;
pub use zamazka::ZamazkaTool;

mod stamp;
pub use stamp::StampTool;

mod watermark_library;

mod watermark_entry;

mod watermark_library_window;

mod watermark_removal;
pub use watermark_removal::WatermarkRemovalTool;

mod flux2_klein;
pub use flux2_klein::Flux2KleinTool;

mod region_edit_v2;

mod ai_editor;
pub use ai_editor::AiEditorTool;
