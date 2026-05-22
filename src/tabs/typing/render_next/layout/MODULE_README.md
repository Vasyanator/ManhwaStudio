# Module: src/tabs/typing/render_next/layout

## Purpose
This directory owns layout-to-raster positioning that is not generic word wrapping.
At present it contains the vertical text raster path used after `wrap/vertical.rs`
has prepared vertical layout text.

## Architecture
`layout::vertical` receives shaped `cosmic-text` runs, inline style spans, line offset
tables, and render parameters from `pipeline.rs`. It converts vertical layout text into
columns and cells, computes visual column positions and optical vertical spacing, then
draws glyph RGBA buffers through the shared `raster.rs` helpers.

Wrapping decisions are upstream in `wrap/`; this module should position and draw the
already prepared layout text. Low-level pixel blending, glyph alpha sampling, and bounds
trimming remain in `raster.rs`.

## Files and submodules
- `mod.rs`: private module wiring and re-export of `VerticalRasterRequest` and
  `render_vertical_text`.
- `vertical.rs`: vertical column/cell collection, optical pair spacing, column direction
  handling, inline glyph overrides, glyph rasterization, and final RGBA assembly.

## Contracts and invariants
- Inputs must come through `VerticalRasterRequest`; do not access typing panel, project,
  canvas, overlay, or storage state from layout code.
- The request's `layout_text`, `layout_line_offsets`, inline spans, and line spacing
  table must describe the same normalized text prepared by `pipeline.rs`.
- Vertical wrapping and paragraph splitting belong in `wrap/vertical.rs`; this module
  must not introduce independent word-wrap policy.
- `VerticalLineDirection` controls column ordering only. Glyph raster semantics should
  stay shared with horizontal rendering where possible.
- Output RGBA must remain unmultiplied and sized as `width * height * 4`.
- Glyph bounds, optical profiles, and blank cells must tolerate missing glyph alpha
  data without panics.
- Coordinate names should stay explicit: column positions, cell tops, glyph origins,
  output pixels, and inline offsets are different spaces.

## Editing map
- To change vertical column positioning, spacing, optical adjustment, or direction
  behavior, edit `vertical.rs`.
- To change how vertical text is split into columns before shaping, edit
  `../wrap/vertical.rs`.
- To change glyph alpha sampling, blending, scaled glyph drawing, or trimming, edit
  `../raster.rs` and audit horizontal/formula callers too.
- To add another layout-to-raster mode, add a focused module here and route it from
  `pipeline.rs` through a typed request struct.
