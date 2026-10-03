/*
FILE HEADER (crates/ms-tools/src/lib.rs)
- Назначение: crate root of `ms-tools` — reusable UI/drawing tooling primitives that are
  not bound to a particular tab. Re-exported by the binary as `crate::tools`, so every
  existing `crate::tools::…` call site keeps its path.
- Layer: ABOVE `ms-canvas` / `ms-widgets`, BELOW the tabs. This crate must never name
  `tabs`, `app` or `launcher`; hosts drive the patch core through `PatchHost`.
- Экспорт:
  - `MaskBrush`: переиспользуемая кисть для рисования бинарной маски в `egui::ColorImage`
    (радиус, hotkeys размера, Shift+wheel, отрисовка курсора, штрихи по сегменту).
  - `fill_polygon_spans`: re-export of `ms_raster::fill_polygon_spans`, the one even-odd
    scanline polygon rasterizer (PS-editor lasso, cleaning tools, the patch core). It lives in
    `ms-raster` (level 0) so lower crates can use it; this path is kept for existing callers.
  - `red_black_sor_sweeps`: the ONE red-black SOR kernel of the project. Shared by the cleaning
    tab's gradient fill and by the patch tool's membrane solve; a second implementation anywhere
    is a defect.
  - `overlay_pixel_for_final_color`: solves the DENSE overlay pixel that reproduces a desired
    final colour over a known, opaque backdrop at an alpha of at least the given coverage.
  - `png_wire`: the ONE codec of the region/mask PNGs exchanged over the AI backend wire
    (`encode_rgba_png`, unmultiplied RGBA8; `encode_mask_png_l8`; `decode_png_luma8` for
    returned masks) with the typed, text-free `PngWireError` / `PngDecodeError`. Callers map the
    errors onto their own localized messages.
  - `patch`: the host-neutral core of the «Заплатка» (patch) tool — selection, gesture, ROI
    geometry, the membrane solve and the outline painting, driven by a host through `PatchHost`.
    The core stops at `(page, rect, rgb, coverage)`; storage is the host's decision.
*/

#![warn(clippy::all)]

// The `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`), mounted crate-wide exactly as
// `src/main.rs` mounts them for the binary: the patch tool's localized labels use the bare
// macro names the extraction tool emits and the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

mod mask_brush;
mod overlay_pixel;
pub mod patch;
pub mod png_wire;
mod sor;

pub use mask_brush::MaskBrush;
pub use overlay_pixel::overlay_pixel_for_final_color;
pub use ms_raster::fill_polygon_spans;
pub use sor::red_black_sor_sweeps;
