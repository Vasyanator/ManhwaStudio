# Module: src/bin/text_render_test

## Purpose
Development-only renderer module for the `text_render_test` diagnostic binary. It exercises
cosmic-text layout, rasterization, shape-aware wrapping, inline bold/italic tags, hyphenation, and
post-render text effects in a small egui preview app.

This directory is not a production entry point. Production typing behavior should live under
`src/tabs/typing/` and be called from application code rather than copied from this test binary.

## Architecture
`src/bin/text_render_test.rs` owns the egui app shell, control state, font discovery, render job
queue, preview texture, and PNG save action. It includes this directory's renderer with
`#[path = "text_render_test/render.rs"]` because the project has no shared library target for
diagnostic binaries.

Rendering runs on a background worker. The GUI thread builds `TextRenderParams`, sends a
token-tagged `RenderJob` over an mpsc channel, polls `RenderResult` without blocking, and updates
the preview texture only for the latest token. The worker calls `render_text_to_image` with real
font files and returns either an RGBA image or a visible error string.

`render.rs` owns the local rendering pipeline: font registration in cosmic-text/fontdb, optional
inline tag parsing, soft hyphen insertion, shape-aware line wrapping, glyph rasterization into an
RGBA buffer, and sequential application of JSON-described effects.

## Files and submodules
- `render.rs`: local renderer implementation used by `src/bin/text_render_test.rs`. It defines
  `TextRenderParams`, `RenderedTextImage`, `HorizontalAlign`, `TextShape`, and
  `render_text_to_image`, plus private helpers for wrapping, hyphenation, raster compositing, and
  effects.

## Contracts and invariants
- Rendering must use actual font files from the resolved `fonts` directory. Missing or unreadable
  fonts must produce visible errors; do not synthesize placeholder output.
- The GUI thread must not run the heavy render path. Keep text layout, glyph rasterization,
  hyphenation, and effect processing on the render worker.
- `RenderedTextImage.rgba` is unpremultiplied RGBA with length `width * height * 4`; any new image
  operation must preserve that shape contract.
- `TextRenderParams.width_px` and font size are clamped to usable positive values before layout;
  public render behavior must not panic on empty text, narrow widths, or missing optional effects.
- Effect JSON is an ordered pipeline. Unknown, malformed, or out-of-range effect values should
  return clear error strings instead of being silently ignored when they would change output.
- Inline style tags are limited to the local parser contract (`b`/`strong`, `i`/`em`, and line
  breaks). New markup behavior should be explicit in both the UI serializer and renderer parser.
- Shape wrapping must keep width/height, line index, byte index, and character boundary handling
  explicit, especially around soft hyphens and Cyrillic hyphenation rules.

## Editing map
- To change the diagnostic UI, render controls, effect cards, font discovery, job queue, preview,
  or PNG save behavior, edit `src/bin/text_render_test.rs`.
- To change renderer inputs, glyph layout, wrapping, hyphenation, RGBA compositing, or effect
  semantics for this diagnostic binary, edit `render.rs`.
- To align this diagnostic with production typing output, compare against `src/tabs/typing/` and
  move reusable production behavior there instead of making this directory the runtime owner.
