# Module: src/tabs/typing/render_next/layout

## Purpose
This directory owns layout-to-raster positioning that is not generic word wrapping.
At present it contains the vertical text raster path used after `wrap/vertical.rs`
has prepared vertical layout text.

## Architecture
`layout::vertical` receives shaped `cosmic-text` runs, inline style spans, line offset
tables, and render parameters from `pipeline.rs`. It converts vertical layout text into
columns and cells, computes visual column positions and optical vertical spacing, then
draws each glyph by rasterizing its true font outline (shared `glyph_blit` helpers +
`vector::rasterize_outline_into`), mirroring the horizontal and on-path render paths.
COLR/bitmap color glyphs with no monochrome outline fall back to the `raster.rs` bitmap
blit.

Layout is swash-bitmap based: column widths, cell baselines, optical profiles, glyph
visual-width measurement, and the output-bounds pass all use `cache.get_image` placement.
Only the final per-glyph draw switched from a bitmap blit to the vector rasterizer, so the
geometry (and the golden output) stays identical apart from anti-aliasing.

Optical kerning (vertical axis): when `params.kerning_mode == KerningMode::Optical`,
`compute_vertical_cell_baselines` additionally nudges the uniform base gap between
two adjacent inked glyphs of a column by a per-pair signed delta. The MINIMUM
DIRECTIONAL top-to-bottom ink whitespace is measured from the glyph outlines placed
through the same draw-pass pivot (`place_optical_vertical_contour` +
`optical_pair_gap` with `OpticalAxis::Vertical`): the smallest
`cur_top(x) - prev_bottom(x)` over the pair's overlapping horizontal band (the
closest facing points; a scanline projection, NOT a Euclidean min-distance). That
min gap is normalized toward the column median gap so the closest points become
uniform, and the same min gap feeds the collision floor. The self-calibrating
median / delta / base-advance math
AND the pair-gap metric are the shared axis-agnostic core in `render_next::optical`
(reused with the horizontal path — no duplicate formula or metric). Every non-Optical
kerning mode keeps `delta == 0` and stays BYTE-IDENTICAL to the pre-optical
stacking. The vertical stacking is ink-height based and never applies font pair
kerning, so `Fixed` and `Auto` coincide on this path (only `Optical` re-spaces);
spaces and line breaks reset the pair chain (a gap is only added between
two consecutive `Glyph` cells), so optical kerning never crosses them. A pair with
no horizontal overlap yields an infinite gap and is not kerned. The measurement
reuses the vertical render's `OutlineCache` + a per-render `OpticalContourCache`.

Approximation caveat: the measure-then-shift is exact on the measured layout but
sub-pixel-approximate on the rendered pixels. The provisional measurement pen mirrors
the draw pass's `origin_x` except the per-column constant `column_x`, yet its
`SubpixelBin` can still differ from the accumulated draw pen by up to ~0.75px/axis.
Since `delta` is applied along the same vertical axis the gap is measured on, the
"final gap >= 0.5px" / "gaps converge to the column median" contracts hold on the
measured layout and are only sub-pixel-approximate on screen.

Wrapping decisions are upstream in `wrap/`; this module should position and draw the
already prepared layout text. Low-level pixel blending, glyph alpha sampling, bounds
trimming, and the color-glyph bitmap blit remain in `raster.rs`.

## Files and submodules
- `mod.rs`: private module wiring and re-export of `VerticalRasterRequest` and
  `render_vertical_text`.
- `vertical.rs`: vertical column/cell collection, optical pair spacing (metric ink-gap
  stacking plus `KerningMode::Optical` ink-whitespace normalization via the shared
  `render_next::optical` core), column direction handling, inline glyph overrides,
  outline-based glyph rasterization (with color-glyph bitmap fallback), and final RGBA
  assembly.

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
