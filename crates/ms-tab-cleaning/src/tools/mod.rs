/*
FILE HEADER (tabs/cleaning/tools/mod.rs)
- Назначение: корневой модуль инструментов клининга.
- Экспорт:
  - `CleaningTool`, `StrokePoint`, `StrokeModifiers` из `base.rs`.
  - Конкретные инструменты вкладки cleaning:
    `ZamazkaTool`, `StampTool`, `PatchTool`, `GradientFillTool`,
    `TextureSynthesisInpaintTool`, `AotInpaintTool`,
    `FluxFillInpaintTool`, `WatermarkRemovalTool`, `ai_editor_tool()` (a
    `region_edit_v2::host::RegionEditHost` over the «ИИ-редактор области» spec) and
    `ai_api_editor_tool()` (the same host over the «ИИ редактирование (API)» spec).
    `PatchTool` in `patch/` is only this tab's HOST for the «Заплатка» tool: the tool itself —
    selection, gesture, ROI/refusal geometry and the gradient-domain solver — lives in
    `ms_tools::patch`, and `patch/` implements its `PatchHost` (canvas geometry, the two
    region loads, the store into the clean overlay). See `patch/MODULE_README.md`.
- Внутренние модули без экспорта:
  - `mask_generation` — the host-neutral core of «Сгенерировать маску»: the backend source
    catalog, its availability rules, the detection worker and the watermark plumbing. Shared
    by `base.rs` (the detached mask-inpaint editors), `region_edit_v2::host` (the hosted
    tools' mask stack) and `watermark_removal`.
  - `region_png` — the localized face of `ms_tools::png_wire`: the one encoder of the region
    and mask PNGs the AOT, Flux-Fill and AI-editor engines send to the backend.
  - `watermark_library` — библиотека измеренных знаков на диске; используется
    режимом «По главе» из `watermark_removal.rs`.
  - `watermark_entry` — мост между движком разложения и библиотекой: приём
    эталонных кадров, отображение вердиктов и подбор записей по подписи знака.
  - `watermark_library_window` — окно управления библиотекой, открываемое из
    инструмента.
  - `region_edit_v2` — the on-canvas region-editing framework (`RegionFrame`, its mask
    layers and its geometry) and the generic host tool over it (`RegionEditHost`, `HostSpec`,
    the `AiEngine` trait). See its own `MODULE_README.md`.
  - `ai_editor` — the «ИИ-редактор области» `HostSpec` and its `engines` catalog: the AI
    engines hosted behind the `AiEngine` trait. FLUX.2 klein, Lama and SDXL Inpaint live there and are no longer `CleaningTool`s of
    their own; `ai_editor::engines::lama` owns the LaMa model catalog that its sibling
    `ai_editor::engines::sdxl` reads for the 4-channel prefill picker.
  - `ai_api_editor` — the «ИИ редактирование (API)» `HostSpec` and its single cloud engine
    over `ms_ai_api::image_edit` (hosted image-edit models; no backend, no Torch).
*/
mod base;

mod mask_generation;

mod region_png;

pub use base::StrokeModifiers;
pub use base::{CleaningCursorOccluder, CleaningTool, StrokePoint};

mod gradient;
pub use gradient::GradientFillTool;

mod texture_synthesis;
pub use texture_synthesis::TextureSynthesisInpaintTool;

mod flux_fill;
pub use flux_fill::FluxFillInpaintTool;

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
pub(crate) use ai_editor::tool as ai_editor_tool;

mod ai_api_editor;
pub(crate) use ai_api_editor::tool as ai_api_editor_tool;
#[cfg(any(test, feature = "test-support"))]
pub use ai_api_editor::suppress_settings_persistence_for_tests as suppress_ai_api_editor_settings_persistence_for_tests;
