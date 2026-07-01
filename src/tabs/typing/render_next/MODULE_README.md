# Module: src/tabs/typing/render_next

## Purpose
This directory is the production text renderer used by the `Text` tab. It converts
`TextRenderParams` into a trimmed RGBA `RenderedTextImage` with optional rich inline
styles, wrapping, shape-aware layout, vertical text, formula/custom-line layouts, and
JSON-driven effects.

The renderer is pure rendering logic. It must not know about `CanvasView`, overlay
placement, `text_info.json`, project storage, or GUI widgets.

Architecture: glyph drawing is vector-first. Monochrome glyphs are rasterized
from font outlines (`vector.rs` + shared pivot helpers in `glyph_blit.rs`) on all
three draw paths — horizontal (`pipeline.rs`), vertical (`layout/vertical.rs`),
and on-path/formula (`formula/render.rs`). `SwashCache::get_image` is kept only
for color/emoji glyphs, the bitmap placement/bounds box that pins the outline
pivot, and bitmap ink measurement.
Shaping/layout/font matching stay on cosmic-text. History, the deferred
direct-`rustybuzz` Phase 4, and the decision not to build a `TextDocument` facade
are recorded in `VECTOR_ENGINE_REFACTOR.md`.

## Architecture
The public boundary is intentionally small:

- `types.rs` defines the stable caller-facing data contract.
- `render_next::render_text_to_image` is the normal entry point.
- `render_next::apply_effects_to_image` applies the same post-effect pipeline to an arbitrary
  RGBA image (no text layout), so callers can reuse stroke/glow/shadow/etc. on imported image
  overlays. It validates `width * height * 4`, treats empty/`[]` effects JSON as a no-op, and
  returns a `RenderedTextImage` that effects may grow for extra margin.
- `pipeline::smoke_render_text_to_image` is only a smoke/contract helper.

The main render flow is:

1. `pipeline.rs` prepares source text (`uppercase_text`, trimming, sentence newlines),
   applies preprocess effects, parses inline style tags, and loads the selected font.
2. `font_registry.rs` registers the selected face and requested inline fonts in
   `cosmic-text`.
3. `wrap/` builds layout text for horizontal, vertical, and shape-aware modes, including
   hyphenation and emergency split rules.
4. `pipeline.rs` routes by `TextLayoutMode`:
   `Normal` uses the horizontal raster path, `Formula`/`Shape` use `formula::render`,
   `CustomRasterLines`/`CustomVectorLines` use drawn/vector line paths, and vertical
   text uses `layout::vertical`.
5. `raster.rs` handles swash glyph sampling, RGBA blending, scaled glyph drawing, bounds
   tracking, cancellation checks, and alpha trimming.
6. `effects/` parses and applies post-effects to the finished RGBA image.

`render_next` is still internally staged, but callers must treat it as the active
renderer contract. Internal modules may be reorganized as long as `types.rs` and
`render_text_to_image` keep their behavior.

## Files and submodules
- `mod.rs`: module wiring, public re-export of `render_text_to_image`, and runtime smoke
  anchors that keep staged contracts compiled.
- `types.rs`: public render parameter/result types and enums shared with `typing/tab.rs`
  and `typing/panel.rs`.
- `pipeline.rs`: central orchestration, horizontal rendering, line metrics, inline glyph
  overrides, shape comparison, cancellation handling, and post-effect application.
- `font_registry.rs`: selected font loading and inline-font registry construction.
- `inline_styles.rs`: parser/remapper for inline tags, attrs-compatible style spans, and
  line-level inline alignment markers.
- `raster.rs`: low-level swash sampling, alpha/source-over blending, glyph drawing,
  bilinear image sampling, and alpha-bounds trimming. Owns the color-glyph bitmap
  fallback and bitmap-based measurement/bounds only; monochrome glyphs on all
  three modes (horizontal, vertical, on-path/formula) are rasterized from outlines
  instead.
- `glyph_blit.rs`: shared outline-blit helpers (`hash_font_id`,
  `resolve_outline_for_glyph`, `glyph_outline_transform`) used by the horizontal
  path (`pipeline.rs`), the vertical path (`layout/vertical.rs`), and the
  on-path/formula path (`formula/render.rs`) so the outline->world pivot lives in
  one place.
- `drawn_lines.rs`: raster layout-line tracing and vector-line path normalization for
  custom line layout modes.
- `glyph_contour.rs`: placement (affine transform + AABB) and minimum-distance geometry
  for glyph ink contours used by on-path minimum-distance spacing. The contours
  themselves are produced by `vector::glyph_contour_from_outline`.
- `vector.rs`: vector-glyph layer for the `VECTOR_ENGINE_REFACTOR.md` move — swash
  outline extraction/flattening + cache, the single zeno coverage-mask rasterizer
  (monochrome tint contract + `blend_pixel_over`), the anti-aliasing coverage->alpha
  transfer table (`build_aa_lut`, applied inside `rasterize_outline_into` before the
  tint multiply), and `Outline`->`GlyphContour`
  conversion. Wired into the on-path / formula / custom-line composite pass
  (`formula/render.rs`), the horizontal path (`pipeline.rs`, including the
  inline-rotated variant), and the vertical path (`layout/vertical.rs`) via the
  shared `glyph_blit.rs` helpers; only color-glyph fallbacks and bitmap
  measurement/bounds still use `raster.rs` bitmaps.
- `wrap/`: text wrapping and hyphenation subsystem.
  See `wrap/MODULE_README.md`.
- `layout/`: layout-to-raster positioning code that is not generic wrapping.
  See `layout/MODULE_README.md`.
- `formula/`: formula and custom-line layout subsystem.
  See `formula/MODULE_README.md`.
- `effects/`: JSON effects subsystem.
  See `effects/MODULE_README.md`.

## Contracts and invariants
- `TextRenderParams` is the only caller-facing input contract. When adding a field or
  enum variant, update `types.rs`, parser/serialization call sites in the parent
  `typing` module, the smoke anchor in `mod.rs`, and focused tests.
- `TextRenderParams.anti_aliasing` (`AntiAliasingMode`) selects a coverage->alpha
  transfer curve applied by the monochrome outline rasterizer only; it does NOT
  affect layout, so it is intentionally excluded from `TextRenderShapeCompareParams`.
  Each render builds the LUT once via `vector::build_aa_lut` next to its
  `OutlineCache` and passes it into every `rasterize_outline_into` call.
  `AntiAliasingMode::Smooth` is the identity table (byte-identical to the pre-AA
  renderer). The color-glyph bitmap fallback path does not go through the LUT.
- `RenderedTextImage.rgba` must always be `width * height * 4` bytes in unmultiplied
  RGBA order. Empty/transparent output must still use valid dimensions where possible.
- Public renderer errors are `Result<_, String>` because callers surface them directly
  in UI status. Include the failing stage or field name in error strings.
- Cancellation is cooperative through `Option<(&Arc<AtomicU64>, u64)>`. Long loops and
  multi-stage operations must check `raster::is_cancelled`; cancellation returns early
  without applying stale work.
- All raster and image helpers must validate dimensions/buffer lengths before indexing.
  Do not add panics for malformed fonts, malformed effects JSON, invalid layout images,
  or bad buffer shapes.
- Keep coordinate units explicit: glyph layout pixels, output image pixels, formula
  curve coordinates, line arc length, and character/style offsets are different spaces.
- Inline style spans use byte offsets after parsing and must be remapped after text
  normalization/wrapping. Do not apply spans from the original tagged text directly to
  reshaped layout text.
- Inline alignment is resolved per layout line from the style span at the line's start
  offset. It affects horizontal placement only; glyph attrs do not carry alignment.
- `TextRenderShapeCompareParams` is a pre-raster optimization contract. It compares
  prepared `layout_text` for shape/wrap parameters and may cancel rendering only when
  `cancel_render_if_layout_text_unchanged` is set.
- Effects JSON is backward-compatible: missing `effect_type` means post-effect, and
  aliases in `effects/parse.rs` are part of the persisted contract.
- Preprocess effects run before inline-style parsing and may generate inline tags.
  Post-effects mutate the final image and must not reach back into layout state.
- Formula expressions must remain finite. Parser/evaluator errors must identify
  unknown variables/functions or the failing `TextFormulaLayoutParams` field.
- Custom raster-line layout reads a PNG path from `TextDrawnLinesLayoutParams`; failures
  should be clear errors, not silent fallback to normal text.

## External Dependencies
- `cosmic-text` provides font database, shaping, layout runs, and swash cache access.
- `hyphenation` provides embedded Russian and English hyphenation dictionaries.
- `image` provides RGBA/gray image containers and blur operations used by effects and
  drawn-line layout.
- `serde_json` is used only inside the effects parser; renderer callers pass effects as
  a JSON string through `TextRenderParams.effects_json`.

## Editing map
- To change caller-visible render parameters or result shape, start in `types.rs`, then
  update `mod.rs` smoke anchors and parent typing serialization/parsing.
- To change normal horizontal rendering, glyph scaling, kerning, hanging punctuation,
  line spacing, shape comparison, or routing, edit `pipeline.rs`. Horizontal
  monochrome glyphs rasterize from outlines via `draw_horizontal_glyph` (normal)
  and the `RotatedGlyphPlacement` draw pass (inline-rotated), both using
  `glyph_blit::glyph_outline_transform`; the outline->world pivot lives in
  `glyph_blit.rs`, not here.
- To change wrapping behavior, edit `wrap/`; keep measurement/scoring in
  `horizontal.rs`, dictionary/safety rules in `hyphenation.rs`, shape profiles in
  `shape.rs`, and vertical pre-layout in `vertical.rs`.
- To change vertical text positioning or optical spacing, edit `layout/vertical.rs`.
- To change formula, shape-path, or custom raster/vector line placement, edit
  `formula/` and `drawn_lines.rs`.
- To change a JSON effect, update `effects/parse.rs`, the concrete effect module, and
  tests for parsing plus image math.
- To change low-level blending, sampling, trimming, or cancellation semantics, edit
  `raster.rs` and audit every caller because those helpers are shared across modes.

## Testing Guidance
- Keep tests close to the helper or subsystem they protect. This module already has
  local unit tests for wrapping, hyphenation, inline styles, formula parser/evaluator,
  raster helpers, effects math, vertical layout, and render routing.
- Add golden or property-style tests for new layout contracts where exact pixels are
  fragile. Use explicit tolerances for floating-point geometry and alpha math.
- After Rust changes, run `cargo check-all` and
  `cargo clippy --all-targets -- -D warnings`.
