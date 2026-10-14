# Refactor plan: vector-glyph-first text engine

Status: **Phases 0-2 IMPLEMENTED.** Vector-first rasterization is the active
draw path for monochrome glyphs on all three modes — on-path/formula,
horizontal, and vertical. Phase 3 was reduced to consolidation + docs (see
"Implemented" below);
the formal `TextDocument`/positioner facade was intentionally NOT built. Phase 4
(dropping cosmic-text for direct `rustybuzz`) remains deferred.

Decision on scope: **keep cosmic-text for shaping / per-run styling / font
matching; replace only the raster back-end with a vector intermediate model.**
Direct-`rustybuzz` shaping (dropping cosmic-text entirely) is explicitly
deferred to a gated Phase 4 and is NOT part of the committed plan.

This document describes the target architecture and the migration that was
carried out. It is the single source for the refactor; keep
`render_next/MODULE_README.md` pointing here rather than duplicating the plan.

---

## 0. Implemented (Phases 0-2)

The migration replaced the bitmap raster back-end with a vector intermediate for
all monochrome (non-color) glyphs while keeping cosmic-text shaping/layout:

- **Phase 0 — on-path / formula.** `vector.rs` extracts and flattens swash
  glyph outlines (cache keyed by font id + glyph id + em) and rasterizes them
  once via a single `zeno` coverage-mask tinter (`rasterize_outline_into`,
  honoring the section-4.1 monochrome tint contract + `blend_pixel_over`). The
  on-path min-distance ink contour is derived from the `Outline`
  (`glyph_contour_from_outline`) instead of tracing a rasterized alpha; the
  old rasterize-then-retrace path was removed.
- **Phase 1 — horizontal.** `pipeline.rs` `draw_horizontal_glyph` and the
  inline-rotated `RotatedGlyphPlacement` draw pass rasterize from outlines,
  using `glyph_blit::glyph_outline_transform` for the outline->world pivot.
- **Phase 2 — vertical + shared plumbing.** `layout/vertical.rs` rasterizes
  vertical column cells from outlines (upright, rot = 0) through the same
  `glyph_blit` helpers; the outline/pivot helpers were lifted into
  `glyph_blit.rs` so the horizontal, vertical, and on-path/formula paths share
  one source of truth for the pivot.

- **Subpixel restoration (outline paths).** cosmic-text quantizes each glyph's
  fractional pen into a 4-way `SubpixelBin` (`{0,0.25,0.5,0.75}` px) and bakes it
  into the swash BITMAP coverage via `Render::offset`, while `physical.x/y` carry
  only the integer pen. The outline paths build their pen from the integer
  `physical.x/y`, so the baked fraction is re-added as a box-local translation
  through the single `subpixel: [f32; 2]` parameter of
  `glyph_blit::glyph_outline_transform` (scaled with the glyph, then rotated with
  it). `glyph_blit::glyph_subpixel_offset` reads it from the same
  `physical.cache_key` every draw site already computes. This is applied on ALL
  outline draw sites (horizontal, inline-rotated, vertical, drawn-line/formula
  composites) AND the min-distance ink contour, so drawn ink and measured contour
  stay consistent. The color-glyph bitmap fallback passes `subpixel = [0.0, 0.0]`
  — `get_image` already baked the fraction into that coverage. Without this,
  every glyph was quantized to an integer pixel (up to 0.75 px per-glyph jitter);
  restoring it brings the outline paths measurably closer to the pre-refactor
  bitmap baseline on every golden case.
- **Outline-less bitmap fallback (unified).** All four draw paths (horizontal,
  inline-rotated, vertical, formula) blit the swash bitmap for ANY outline-less,
  non-empty glyph — not just `SwashContent::Color`. A monochrome glyph with an
  embedded bitmap but no fillable outline (sbix / CBDT-mono) is drawn instead of
  silently dropped. The empty-glyph (`glyph_w==0||glyph_h==0`) skip stays.

All three exercised modes (horizontal, vertical, on-path/formula) now rasterize
monochrome glyphs from font outlines via `vector.rs` + `glyph_blit.rs`. Parity
with the old bitmap blit is IoU ~= 1.0,
verified by the golden gallery (`bin/render_gallery`, 9 cases): the vector draw
path is byte-identical to the pre-refactor reference for the covered cases.

`SwashCache::get_image` is intentionally retained for: (a) color/emoji glyphs
(`SwashContent::Color`, no fillable outline) as the bitmap fallback; (b) the
bitmap **placement box** (left/top/width/height) that feeds
`glyph_outline_transform` so the outline lands on exactly the pixels the bitmap
blit used, plus the horizontal/formula/vertical **bounds** passes; and (c)
bitmap ink **measurement** still used by layout (vertical visual-width, formula
`glyph_ink_profile`, and the min-distance ink extent in
`formula::render::seed_ink_geometry`).

**Phase 3 decision — no `TextDocument` facade.** The design's Phase 3 proposed a
formal `TextDocument` + `GlyphPositioner` IR. It was intentionally NOT built:
`glyph_blit.rs` already consolidates outline resolution and the outline->world
pivot across all three modes, and `vector.rs` already unifies rasterization, so
the facade would add an abstraction layer without a functional gain (speculative
per the "no future-proofing" rule). Positioners stay as the existing per-mode
functions.

---

## 1. Problem ("the elephant")

`render_next` is **bitmap-first**. Every glyph is obtained as a raster bitmap
via `SwashCache::get_image`, and ALL geometry — kerning, on-path spacing,
inter-glyph distance — is computed on the rasterized alpha. The clearest symptom
is the `MinimumPreviousDistance` on-path mode: it rasterizes a glyph and then
re-traces a *pixel* contour (`glyph_contour::trace` over alpha) even though the
font already has a true vector outline. That is a rasterize-then-retrace round
trip.

We want: build the layout from vector glyphs, move / rotate / measure distances
while still vector, and rasterize once at the end — while preserving per-inline-
tag base color and all existing effects.

## 2. What cosmic-text actually does for us (inventory)

cosmic-text is used for exactly five things; everything else is already app-owned.

Relied upon (keep):
1. **Shaping** (`Shaping::Advanced`): glyph ids, advances (`glyph.w`), pen
   positions (`glyph.x`), and stable source **byte ranges** (`glyph.start/end`).
2. **Per-run styling** via `Buffer::set_rich_text` + `Attrs` (bold/italic/named
   family/per-run em size through `metrics_opt`).
3. **Rasterization** via `SwashCache::get_image` (the only outline access path
   today, indirectly).
4. **Width measurement** for the wrap engine (throwaway buffers summing
   `layout_runs().line_w`).
5. **Font loading / family matching** via `fontdb` (`font_registry.rs`).

NOT relied upon (already app-owned or unused):
- Line breaking / word wrap: `Wrap::None`; the app pre-breaks with `\n`
  (`wrap/horizontal.rs`, `wrap/vertical.rs`).
- Alignment: only `Align::Justified` is delegated; left/center/right are app
  pixel offsets.
- Bidi: `run.rtl` / glyph level never read; app assumes LTR visual order.
- Multi-script fallback: `font_registry.rs` pins all generic families to the
  selected face, so real fallback is minimal.
- Metrics: line height and baselines are app-provided, not read from the font.
- Hyphenation: `hyphenation` crate + `segmentation/` heuristics, not cosmic.
- Optical / ink kerning: `glyph_contour.rs` + `formula/render.rs`.

Target languages: Cyrillic + Latin (+ SFX fonts). No CJK vertical typography or
complex-script joining is exercised. A shaper replacement (Phase 4) would only
need Cyrillic+Latin, but we are NOT doing that now.

## 3. Target architecture

```
shaping (cosmic-text: glyph_id, advance, pen.x, byte-range, per-run Attrs)
        |
        v
Vector IR:  TextDocument { lines/columns of GlyphInstance }
        |  positioners (translate/rotate/scale vector glyphs; measure on outline)
        |  -- horizontal · vertical · on-path · formula · shape-fit --
        v
single rasterizer (zeno Mask at final float coords + tint + over-blend) -> one RGBA canvas
        |
        v
effects (unchanged: whole-image post-effects, JSON order; preprocess stays pre-layout)
        |
        v
downstream deform-mesh in tabs/typing/mask.rs (unchanged pixel warp)
```

The "universal layout engine" is the **common IR + pluggable positioners**, not
a new shaper. Each layout mode produces the same `TextDocument`; one rasterizer
consumes it.

### 3.1 Intermediate representation (sketch — finalize during Phase 0)

```rust
/// One shaped glyph carried through layout as vector data until final raster.
struct GlyphInstance {
    /// Vector outline in glyph-local em-scaled px (y-down), from swash
    /// `Scaler::scale_outline`. `None` only for color/bitmap glyphs.
    outline: Option<Outline>,
    /// Bitmap fallback for COLR/CBDT/emoji glyphs (SwashContent::Color), which
    /// have no fillable outline. Exactly one of `outline`/`color_bitmap` is set.
    color_bitmap: Option<ColorBitmapGlyph>,
    /// Resolved inline base color; applied with the preserved tint contract.
    color: [u8; 4],
    /// Stable source byte range (from cosmic `glyph.start/end`) — post-shaping
    /// effects (color, offset, rotation, kerning, hanging punctuation, soft
    /// hyphen) map back to source through this. MUST survive layout.
    source_bytes: core::ops::Range<usize>,
    /// Shaping-derived nominal advance (px) for wrap/kerning math.
    advance_px: f32,
    /// Filled by the positioner; identity until placed.
    transform: GlyphTransform,
}

/// Local->world placement: world = Rot(rot) * (Scale(sx,sy) * local) + pos.
/// Same convention as glyph_contour::PlacedContour so measurement is reusable.
struct GlyphTransform { pos: [f32; 2], rot: f32, scale: [f32; 2] }

/// A flattened outline: closed subpaths of points in glyph-local px, plus the
/// fill winding rule. Beziers are flattened to a tolerance at build time.
struct Outline { subpaths: Vec<Vec<[f32; 2]>>, winding: FillRule }

struct TextDocument {
    /// Rows (horizontal) or columns (vertical) of placed glyphs, in draw order.
    groups: Vec<Vec<GlyphInstance>>,
    /// Axis-aligned content origin, as today (RenderedTextImage.content_origin).
    content_origin: [f32; 2],
}
```

Positioner trait (one per mode), consuming shaped glyphs + advances and writing
`transform` on each `GlyphInstance`:

```rust
trait GlyphPositioner {
    fn place(&self, doc: &mut TextDocument, ctx: &LayoutContext);
}
// impls: HorizontalPositioner, VerticalPositioner, OnPathPositioner,
//        FormulaPositioner, ShapeFitPositioner (shape-fit stays in wrap/).
```

Distance/measurement (`glyph_contour::PlacedContour`, `min_placed_distance`)
stays, but `PlacedContour` is derived from `Outline` (flatten -> simplify),
NOT from a rasterized alpha. `glyph_contour::trace(alpha,...)` is removed once
the on-path mode is migrated.

### 3.2 Single rasterizer

For each `GlyphInstance` in draw order:
- outline glyph: transform the flattened subpaths to world float coords, fill to
  an 8-bit coverage mask with `zeno::Mask`, tint with `color` using the exact
  contract in section 4, and composite with `raster::blend_pixel_over`.
- color/bitmap glyph: keep the existing swash bitmap path (get_image) — tint via
  channel-multiply as today.

This collapses the three current blit paths (`rasterize_unscaled_glyph`,
`draw_scaled_glyph_rgba`, `draw_rotated_scaled_glyph_rgba`) into one, and gives
subpixel positioning for free (fill at exact float coords instead of subpixel
cache bins).

## 4. Contracts that MUST be preserved

### 4.1 Inline base color (from `raster::sample_swash_pixel`)
Per glyph a resolved `[u8;4]` is applied as:
- `Mask` / `SubpixelMask` (monochrome outline glyph): output RGB is **replaced**
  by `color[0..3]`; output alpha = `coverage * color[3]/255`.
- `Color` (emoji/COLR bitmap): output RGB is **channel-multiplied** by
  `color[0..3]/255`; output alpha = `srcA * color[3]/255`.
Color is resolved by source byte offset (`inline_text_color_for_glyph` ->
`inline_text_color_at_offset`), so it survives shaping/reorder. Soft-hyphen and
formula-seed glyphs each resolve their own color. The vector rasterizer applies
the identical semantics; only the coverage source changes (zeno mask vs swash
bitmap) for outline glyphs.

### 4.2 Effect stages (from `effects/`)
- `preprocess` (currently only `text_shake`) runs on the raw string BEFORE
  layout, emitting inline `<offset>` tags. Must stay pre-layout, unchanged.
- Post-effects run ONCE on the fully assembled straight-RGBA image, in JSON
  array order, each mutating the image (several grow the canvas and update
  `content_origin`). Every post-effect consumes only the assembled alpha/RGBA —
  vector-first does not change them. Keep the assembled-image contract.
- `stroke` operates on the alpha edge, NOT the glyph outline. A true outline
  stroke would be a behavior change — keep alpha-based unless explicitly opted in.

### 4.3 Output shape
A single axis-aligned straight-alpha RGBA8 buffer plus `content_origin_x/y`,
trimmed to alpha bounds at the end (`trim_rendered_image_to_alpha_bounds`). The
downstream deform mesh in `tabs/typing/mask.rs` warps this raster and is out of
scope.

### 4.4 Stable per-glyph byte ranges
All post-shaping inline effects map shaped glyph -> source bytes via
`glyph.start`. The IR must carry `source_bytes`. Since shaping stays on
cosmic-text, these remain available.

## 5. Dependencies

Almost no new crates:
- Outlines: `swash::scale::Scaler::scale_outline` — `swash 0.2.6` is already in
  the lock (transitive via cosmic-text); promote to a direct `Cargo.toml` dep.
  Alternative with zero manifest change: `cosmic_text::ttf_parser` `OutlineBuilder`.
- End rasterization: `zeno 0.3.3` `Mask` (already pulled by swash; `eval`
  feature on). `cosmic_text::{Command, Placement, Transform}` path vocab is
  re-exported if useful.
- Shaping stays `cosmic-text` (`Buffer::layout_runs()` for glyph ids + positions
  + byte ranges). `cosmic_text::rustybuzz` exists if Phase 4 ever happens.
No `resvg`/`usvg`/`lyon`/`kurbo`/`tiny-skia` direct dep required.

## 6. Phased migration

Golden-image regression at every step using the existing `bin/text_render_test`
reference binary (add golden fixtures for representative cases: horizontal
justified paragraph, vertical SFX, on-path curved line, inline color + bold +
per-run size, emoji/color glyph, soft-hyphen wrap).

- **Phase 0 — Vector IR + rasterizer, no layout change.**
  Introduce `GlyphInstance`/`TextDocument`/`Outline`/`GlyphTransform`, swash
  outline extraction (cache keyed by font id + glyph id + em size; no subpixel
  bin needed for vectors), and the single zeno-based rasterizer honoring the
  section-4.1 tint contract and the color-glyph bitmap fallback. Migrate ONE
  mode: on-path/formula. Derive `PlacedContour` from `Outline` and delete the
  raster-and-trace path in the min-distance feature. Golden-diff vs current.
- **Phase 1 — Horizontal on IR.** `HorizontalPositioner` emits `GlyphInstance`s
  (baseline, align, optical/inline kerning, soft-hyphen); shared rasterizer.
  Golden-diff. Retire the horizontal blit passes.
- **Phase 2 — Vertical on IR.** `VerticalPositioner` (cell advance, column
  width, positional glyph->char matching preserved). Golden-diff.
- **Phase 3 — Consolidation (DONE, facade dropped).** The universal
  `TextDocument`/positioner facade was NOT built (speculative; `glyph_blit.rs` +
  `vector.rs` already consolidate outline resolution, pivot, and rasterization
  across modes). Phase 3 was instead a cleanup + docs pass: audit dead code,
  audit remaining `get_image` sites, and record the achieved architecture. All
  three modes rasterize monochrome glyphs from outlines; `get_image` is kept only
  for color glyphs, the placement/bounds box, and ink measurement (see
  section 0).
- **Phase 4 — DEFERRED / gated.** Evaluate replacing cosmic-text shaping with
  direct `rustybuzz` + an app-owned per-run/justify/fallback layer. Only if
  0–3 prove the IR and a concrete need appears. Highest risk (per-run styling,
  justify, byte-offset stability, fallback) — do not start without a fresh
  decision.

## 7. Risks and mitigations

1. **Color/emoji glyphs have no outline** — `SwashContent::Color` (COLR/CBDT).
   MITIGATION: hybrid IR (`color_bitmap` fallback via existing get_image path).
   This is mandatory, not optional.
2. **Hinting**: swash bitmaps may be hinted; outlines are not. Small body text
   may differ slightly. MITIGATION: golden thresholds with tolerance; the app's
   text is mostly large SFX where hinting is irrelevant.
3. **AA parity** zeno vs swash: close (swash uses zeno internally). Verify via
   golden.
4. **Byte-range stability** for post-shaping effects. Kept because shaping stays
   on cosmic-text.
5. **Stroke semantics** (alpha-edge vs outline): keep alpha-based; outline stroke
   is a separate opt-in feature, not part of this refactor.
6. **Deform mesh** stays a downstream pixel warp. Future opportunity (out of
   scope): deform vector before raster for higher quality.

## 8. Out of scope
- Dropping cosmic-text shaping (Phase 4, gated).
- Vector deformation before raster.
- Changing effect behavior (including true outline stroke).
- CJK vertical typography / complex-script shaping / bidi.

## 8b. Anti-aliasing transfer curve (shipped)
Because outline coverage is unhinted and softer than the old hinted bitmaps,
`TextRenderParams.anti_aliasing` (`AntiAliasingMode`: None/Sharp/Crisp/Strong/Smooth)
selects a coverage->alpha transfer curve applied inside `rasterize_outline_into`
BEFORE the tint multiply. `vector::build_aa_lut` builds a `[u8; 256]` LUT once per
render (next to each `OutlineCache`) and every outline draw site passes it through.
`Smooth` is the exact identity table, so it is byte-identical to the pre-AA output
(regression anchor verified by the `render_gallery` `smooth` run). `None` is a hard
threshold at coverage 0.5; `Sharp`/`Crisp` are symmetric contrast curves; `Strong`
adds a positive bias so mid coverage is denser. The color-glyph bitmap fallback is
not affected. AA does not affect layout, so it is excluded from
`TextRenderShapeCompareParams`. Panel default is `Strong`.

## 9. Anchor files
`pipeline.rs`, `raster.rs`, `glyph_contour.rs`, `font_registry.rs`,
`inline_styles.rs`, `wrap/{shape,horizontal,vertical,forms,hyphenation}.rs`,
`layout/vertical.rs`, `formula/render.rs`, `effects/*`,
`src/tabs/typing/segmentation/{base,ru}.rs`, and the reference example
`crates/ms-text-render/examples/text_render_test/render.rs`.
