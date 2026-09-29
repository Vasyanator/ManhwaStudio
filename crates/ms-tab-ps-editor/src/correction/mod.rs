/*
File: tabs/ps_editor/correction/mod.rs

Purpose:
Module root of the PS editor's «Коррекция» — a VIEW-ONLY correction of what the canvas shows, so a
user can spot small colour differences. It belongs to the same family as the «Сглаживание» and
«Сетка пикселей» toggles: it changes the PICTURE ON SCREEN and nothing else. Layer pixels,
`layers.json`, `CleanOverlaysModel` and the saved project are never touched.

Submodules:
- `model`: the data model and all the maths. GUI-free and GL-free; holds the maths tests.
- `shader`: paints the correction as an `egui-shader-layers` preset layer and reports whether
  the library can render it. Owns no GL objects (the binary installs / destroys the backend).
- `ui`: the dock-tab body and the reusable per-kind parameter card.

Notes:
Contract and rationale: `MODULE_README.md` next to this file.
*/

pub mod model;
pub mod shader;
pub mod ui;

pub use model::CorrectionState;
pub(crate) use shader::{CorrectionAvailability, paint_correction_layer};
pub(crate) use ui::correction_panel_body;
