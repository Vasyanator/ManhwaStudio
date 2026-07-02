/*
File: src/tabs/typing/render_next/layout/mod.rs

Purpose:
Каркас layout-подсистемы нового рендера typing.

Main responsibilities:
- держать общие layout-алгоритмы отдельно от wrapping и raster;
- стать точкой роста для horizontal/vertical positioning и shape-specific layout.
*/

mod vertical;

pub(crate) use vertical::{VerticalRasterRequest, render_vertical_text};
