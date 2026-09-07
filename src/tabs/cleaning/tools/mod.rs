/*
FILE HEADER (tabs/cleaning/tools/mod.rs)
- Назначение: корневой модуль инструментов клининга.
- Экспорт:
  - `CleaningTool`, `StrokePoint`, `StrokeModifiers` из `base.rs`.
  - Конкретные инструменты вкладки cleaning:
    `ZamazkaTool`, `StampTool`, `PatchTool`, `GradientFillTool`,
    `TextureSynthesisInpaintTool`, `LamaInpaintTool`, `LamaMpeInpaintTool`,
    `AotInpaintTool`, `SdxlInpaintTool`, `FluxFillInpaintTool`,
    `WatermarkRemovalTool`, `AiEditorTool`.
    `PatchTool` in `patch/` is only this tab's HOST for the «Заплатка» tool: the tool itself —
    selection, gesture, ROI/refusal geometry and the gradient-domain solver — lives in
    `crate::tools::patch`, and `patch/` implements its `PatchHost` (canvas geometry, the two
    region loads, the store into the clean overlay). See `patch/MODULE_README.md`.
- Внутренние модули без экспорта:
  - `watermark_library` — библиотека измеренных знаков на диске; используется
    режимом «По главе» из `watermark_removal.rs`.
  - `watermark_entry` — мост между движком разложения и библиотекой: приём
    эталонных кадров, отображение вердиктов и подбор записей по подписи знака.
  - `watermark_library_window` — окно управления библиотекой, открываемое из
    инструмента.
  - `region_edit_v2` — the on-canvas region-editing framework (`RegionFrame`, its mask
    layers and its geometry). Consumed by `ai_editor`; see its own `MODULE_README.md`.
  - `ai_editor::engines` — the AI engines hosted by `AiEditorTool` behind the `AiEngine`
    trait. FLUX.2 klein lives there and is no longer a `CleaningTool` of its own.
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

mod patch;
pub use patch::PatchTool;

mod watermark_library;

mod watermark_entry;

mod watermark_library_window;

mod watermark_removal;
pub use watermark_removal::WatermarkRemovalTool;

mod region_edit_v2;

mod ai_editor;
pub use ai_editor::AiEditorTool;
