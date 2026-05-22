# Module: src/tabs/typing/render_next/wrap

## Purpose
This directory owns text wrapping before rasterization for the production typing
renderer. It prepares layout text for horizontal, shape-aware, and vertical modes while
keeping dictionary hyphenation and emergency split rules out of glyph drawing code.

## Architecture
`mod.rs` exposes the small internal wrap boundary used by `pipeline.rs`:
`reshape_text_for_shape`, `build_vertical_layout_text`, hyphenation dictionaries, and
wrap-mode policy helpers.

The main data flow is:

1. `pipeline.rs` normalizes text, resolves inline styles, font metrics, and wrap mode.
2. `mod.rs` maps `TextWrapMode` to a `WordBreakPolicy` and dictionary requirements.
3. `horizontal.rs` scores candidate line breaks for free/rectangle/shape targets.
4. `hyphenation.rs` supplies safe dictionary and emergency split points.
5. `shape.rs` builds rectangle/oval/hexagon width profiles and iteratively rewraps
   horizontal lines.
6. `vertical.rs` prepares newline-separated vertical columns for the vertical raster
   path.

This module returns layout text and warnings only. It does not draw glyphs, allocate
output images, or apply effects.

## Files and submodules
- `mod.rs`: internal public surface, wrap-mode policy mapping, shared constants, and
  hanging-punctuation classification.
- `horizontal.rs`: DP/scored paragraph wrapping, line-width measurement, candidate
  break collection, keep-together heuristics, and target-width scoring.
- `hyphenation.rs`: embedded Russian/English dictionaries, soft-hyphen insertion,
  safe split filtering, dictionary split lookup, and emergency split fallback.
- `shape.rs`: shape width profiles for rectangle/oval/hexagon, soft peak no-tree ordering,
  iterative horizontal reshaping, and approximate-shape warnings.
- `vertical.rs`: vertical column preparation, paragraph splitting, shape-aware vertical
  targets, and vertical emergency token splitting.

## Contracts and invariants
- Wrapping uses normalized text from `pipeline.rs`; inline style byte-offset remapping
  must happen outside or around this module, not by applying original tagged spans here.
- `TextWrapMode::None` must preserve caller text except for upstream normalization.
- `WholeWords` must avoid dictionary/emergency splitting. Minimal, Moderate, and
  Aggressive modes may use increasingly permissive split policy.
- Dictionary and emergency splits must respect safe text boundaries and must not split
  inside invalid UTF-8 or produce empty head/tail fragments.
- Shape wrapping returns warnings when it uses approximate fallback behavior; do not
  hide those warnings.
- `TextShape::Rectangle`, `Oval`, and `Hexagon` profiles must keep line widths positive
  and respect `shape_min_width_percent`; `SoftPeak` ignores that minimum-width slider and uses
  `shape_variant` to bias among valid no-tree layouts while preserving nondecreasing line units up
  to the middle and nonincreasing units after it.
- Vertical wrapping prepares columns only; glyph positioning and optical spacing belong
  in `../layout/vertical.rs`.
- Measurement through `cosmic-text` is allowed for scoring, but this module must not
  rasterize glyphs or mutate output images.

## Editing map
- To change wrap-mode semantics or shared constants, edit `mod.rs` and update focused
  tests for mode mapping.
- To change horizontal line scoring, candidate generation, keep-together rules, or
  target-width balancing, edit `horizontal.rs`.
- To change language dictionaries, safe split rules, soft hyphenation, or emergency
  split behavior, edit `hyphenation.rs`.
- To change rectangle/oval/hexagon shaping or shape fallback warnings, edit `shape.rs`.
- To change vertical column preparation, edit `vertical.rs`; edit `../layout/vertical.rs`
  only for glyph placement after wrapping.
