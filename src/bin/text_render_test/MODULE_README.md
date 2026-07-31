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
- The `FontSystem` comes from `ms_text_render::new_render_font_system()`, i.e. the SAME
  deterministic bundled `fonts/ui` base production renders on. `FontSystem::new()` must not come
  back: it loads the operator's installed fonts, so this diagnostic would resolve missing glyphs
  and unserviceable style requests through faces the app can never use, and show a picture
  production cannot produce (`dev-docs/unicode_base_font_plan.md`, decision 1).
- A real italic (the `Курсив` flag or a bare inline `<i>`) that no registered family can serve is
  a visible ERROR here, not a silent upright render — `Style::Italic` is a hard match filter in
  cosmic-text and would otherwise take the selected font out of the run entirely. The check is
  `ms_text_render::family_has_matching_face`. This diagnostic deliberately does NOT reimplement
  the production renderer's faux-italic degradation.
- A real BOLD (the `Жирный` flag or a bare inline `<b>`) is guarded the same way, by the weight
  predicate `ms_text_render::family_has_face_of_requested_weight`. Both predicates are the very
  ones production applies before any attrs modification, so the diagnostic accepts exactly the
  style requests production can serve with a real face. Missing one of them is not cosmetic: an
  unserviceable `Weight::BOLD` silently restyles the run into another family that HAS a 700 face
  (on the bundled base `Noto Sans Bold`) and drops every 400-weight fallback font out of the run,
  so rare glyphs become tofu. Production degrades such a request to faux bold; this diagnostic has
  no faux synthesis and reports the error instead.
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
