/*
FILE HEADER (tabs/cleaning/tools/mod.rs)
- Назначение: корневой модуль инструментов клининга.
- Экспорт:
  - `CleaningTool`, `StrokePoint`, `StrokeModifiers` из `base.rs`.
  - Конкретные инструменты вкладки cleaning:
    `ZamazkaTool`, `StampTool`, `GradientFillTool`, `TextureSynthesisInpaintTool`,
    `LamaInpaintTool`, `LamaMpeInpaintTool`, `AotInpaintTool`, `SdxlInpaintTool`.
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
