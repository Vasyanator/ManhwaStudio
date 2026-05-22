# Module: src/tabs/typing/render_next/formula

## Purpose
This directory implements formula-driven and custom-line text layout for the production
typing renderer. It turns parsed formula parameters, raster line paths, or vector line
paths into glyph placement on curves before delegating glyph sampling to the shared
raster helpers.

## Architecture
`mod.rs` re-exports the formula render boundary used by `pipeline.rs`. Formula rendering
is split into three responsibilities:

1. `parser.rs` tokenizes and parses ASCII math expressions into a small AST.
2. `eval.rs` compiles `TextFormulaLayoutParams` expressions and evaluates finite
   transforms or arc-length samples for runtime glyph variables.
3. `render.rs` shapes text, builds glyph seeds, maps glyphs to formula/custom-line
   positions, draws rotated glyphs, and reports when shape mode should fall back to the
   standard text path.

Custom raster-line and vector-line modes share this rendering path because they also
place glyphs by distance along a curve. `drawn_lines.rs` lives one level up and supplies
the line paths.

## Files and submodules
- `mod.rs`: private module wiring, smoke contract, and re-exports for renderer callers.
- `parser.rs`: tokenizer, AST types, recursive-descent parser, operator precedence, and
  function-call parsing for formula expressions.
- `eval.rs`: compiled formula bundle, runtime variable lookup, finite-value checks,
  transform evaluation, tangent rotation, and arc-length table generation.
- `render.rs`: formula/custom-line render requests, glyph seed collection, advance
  assignment, line-path mapping, rotated glyph bounds/drawing, and fallback decisions.

## Contracts and invariants
- Formula input comes from `TextFormulaLayoutParams`; do not read panel state,
  `text_info.json`, project files, or GUI state in this module.
- Expressions are ASCII math with explicit variables/functions. Parser and evaluator
  errors must identify the failing field, token, variable, or function.
- All evaluated coordinates, rotations, and arc lengths must be finite. NaN or infinity
  is an error, not a clamped value.
- `t_start`, `t_end`, scale, offsets, user vars, glyph index variables, line variables,
  width, and font size are separate runtime inputs. Keep their meanings explicit.
- Formula and custom-line rendering must preserve inline style, inline font, kerning,
  glyph scale, glyph offset, and text color overrides supplied by the main pipeline.
- `FormulaRenderOutcome::FallbackToStandard` is an explicit layout decision for modes
  that cannot use a curve safely. Do not silently render a different mode.
- Rotated raster output must keep `RenderedTextImage.rgba` in unmultiplied RGBA order
  with a valid `width * height * 4` buffer.

## Editing map
- To add formula syntax, edit `parser.rs`, then update `eval.rs` if the new syntax
  needs evaluation support and add parser/evaluator tests.
- To add variables or functions, update `eval.rs` and make error messages name unknown
  identifiers clearly.
- To change curve sampling, tangent rotation, or finite checks, edit `eval.rs` and
  verify formula render callers still receive useful errors.
- To change glyph placement along formula, raster-line, or vector-line paths, edit
  `render.rs`.
- To change the public formula parameter contract, start in `render_next/types.rs`,
  then update this module, the smoke anchor in `mod.rs`, and typing serialization.
