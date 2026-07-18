# Module: crates/ms-text-render/src (re-exported as `tabs::typing::render_next`)

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
- `font_provider.rs`: the caller-supplied font source. Fonts reach the render
  path by WORKING NAME through the `FontProvider` trait (`FontContent` payload,
  `FontContentSet` in-memory impl, `font_content_id` cache-key hash). The renderer
  NEVER reads a font file itself; the caller (typing tab) owns the provider (lazy
  file read today, virtual fonts later). Groundwork for virtual/composed fonts.
- `font_registry.rs`: selected/inline font loading and inline-font registry
  construction. The core loader `load_font_content` takes a resolved `FontContent`
  (bytes + face + content id) and never touches the filesystem;
  `load_selected_font_from_path` is a thin compat wrapper over it for the
  path-based forms-metric measurement path. Loading is cache-gated through
  `FontFaceCache` (see `font_system_pool.rs`) keyed by `content_id` so a reused
  `FontSystem` does not accumulate duplicate faces.
- `font_system_pool.rs`: process-global checkout pool of reusable
  `cosmic_text::FontSystem` instances (+ their `FontFaceCache`). Owns
  `with_leased_font_system` (used by `pipeline::render_text_to_image`) and
  `prewarm_font_system_pool` (re-exported for the app to call from a background
  thread).
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
  tint multiply), and `Outline`->`GlyphContour`. Allocation reuse: `OutlineCache`
  owns the reusable swash `ScaleContext` (extraction on a miss takes `&mut context`
  instead of building a fresh one), and `RasterScratch` holds the rasterizer's
  per-glyph buffers (subpath polylines, zeno commands, coverage mask). Each draw
  path creates ONE `RasterScratch` next to its `OutlineCache::new()` and threads
  `&mut scratch` into every `rasterize_outline_into` call. `RasterScratch` resets
  per glyph WITHOUT freeing capacity and re-zeroes the coverage mask, so buffer
  reuse is byte-identical to a fresh allocation.
  conversion. Wired into the on-path / formula / custom-line composite pass
  (`formula/render.rs`), the horizontal path (`pipeline.rs`, including the
  inline-rotated variant), and the vertical path (`layout/vertical.rs`) via the
  shared `glyph_blit.rs` helpers; only color-glyph fallbacks and bitmap
  measurement/bounds still use `raster.rs` bitmaps.
- `extra_info.rs`: pure geometry core for the optional "extra render info"
  payload (`RenderExtraInfoRequest` -> `RenderedTextExtraInfo`). Owns
  `ExtraInfoAccumulator` (collects per-glyph placement-box corner/center samples
  in content space; a true no-op when nothing is requested), the monotone-chain
  convex hull + polygon area centroid (mean center, point-average fallback for
  degenerate hulls), the per-axis median (median center), `finish` which maps
  the accumulated samples from content space to final-image pixels, and the shared
  `rotated_box_samples` helper (a scaled glyph box's rotated corners/center) used by
  the rotated draw paths. Knows nothing about fonts/layout/raster. Unit-tested in
  place.
- `optical.rs`: axis-agnostic pure numeric core for optical kerning
  (`median_of_gaps`, `optical_delta`, `optical_base_advance`) plus the shared
  directional gap metric (`optical_pair_gap` over `OpticalAxis`, returning the
  minimum facing gap as `f32`, `f32::INFINITY` when non-kernable), the
  `OpticalContourCache` type, and the
  simplify-tolerance / min-gap-floor constants. Exactly one source of truth for
  the optical spacing math AND the pair-gap measurement, reused by the horizontal
  path (`pipeline.rs`) and the vertical path (`layout/vertical.rs`). Only the
  contour PLACEMENT (the exact draw-pass transform) stays in the axis-specific
  callers; the scanline gap metric itself lives here. Unit-tested in place.
- `wrap/`: text wrapping and hyphenation subsystem.
  See `wrap/MODULE_README.md`.
- `layout/`: layout-to-raster positioning code that is not generic wrapping.
  See `layout/MODULE_README.md`.
- `formula/`: formula and custom-line layout subsystem.
  See `formula/MODULE_README.md`.
- `effects/`: JSON effects subsystem.
  See `effects/MODULE_README.md`.

## Contracts and invariants
- FontSystem pool (`font_system_pool.rs`): `render_text_to_image` wraps its whole
  body in `with_leased_font_system`, which leases a reusable `FontSystem` + its
  `FontFaceCache` from a process-global pool instead of building a fresh
  `FontSystem` per render (the fresh build ran a full system-font scan every
  call). The pool must preserve BYTE-IDENTICAL output across reuse: font loads go
  through `FontFaceCache` (keyed by content id), so a reused system reuses the
  already-loaded faces instead of re-registering duplicates, and default families
  are set every render for deterministic matching. Two determinism guards keep a
  reused system byte-identical to a fresh one:
  - Pristine default restoration: `FontFaceCache::for_system` captures the fresh
    system's five generic default-family names (sans-serif/serif/monospace/
    cursive/fantasy) once at creation. `font_registry::apply_default_families`
    installs the selected face's family as all five defaults when it HAS a family
    name, and RESTORES the captured pristine names when it does NOT — so a
    no-family face never inherits a prior render's family from the reused db.
  - Taint-and-drop: font matching is by family name. If two DIFFERENT contents
    (different content id) declare the same `(family, weight, style, stretch)`,
    `Family::Name` resolution becomes history-dependent. The loader marks the
    cache `tainted` on such a collision (logged once via `runtime_log::log_warn`)
    and `return_to_pool`/`should_requeue` DROP a tainted system so it can never
    serve a future render. Documented residual: the single render that first
    triggers the collision may still mis-match before the system is dropped (rare,
    self-healing). Stale colliding faces are NOT removed even though fontdb
    exposes `remove_face`, because both colliding files may be used together as
    inline fonts in one render, so removing one file's faces could break that
    file's own text; taint-drop is the safe bound.
  Growth is bounded — a leased system is dropped instead of requeued once its
  cache exceeds `MAX_CACHED_FILES` or the pool holds `MAX_POOLED_SYSTEMS`. The
  renderer must not panic while a system is leased (a panic leaks that one system;
  the pool recreates it). `prewarm_font_system_pool` (re-exported) lets the app
  pay the first scan on a background thread. The pool is NOT used by the throwaway
  metric-measurement `FontSystem` in the typing panel, which passes its own
  one-shot `FontFaceCache::new()` (no pristine defaults captured — single-use, so
  no restore is needed).
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
- `TextRenderParams.global_rotation_deg` rotates the WHOLE laid-out block while
  still vector (glyph outlines), before rasterization — the crisp analog of the
  Ctrl+wheel overlay post-rotation. It applies in EVERY layout mode at the vector
  level, using one shared pass (`raster::RigidPlacement` +
  `rotate_placements_about_centroid`): every glyph placement is rotated rigidly
  about the layout centroid and the angle is added to each glyph's own rotation,
  then the canvas grows to the rotated bounds (no clipping). Per mode: horizontal
  routes through `render_horizontal_rotated`; formula/on-path/shape and custom
  raster/vector lines rotate their `transforms` in `formula/render.rs` (custom
  vector lines drop their fixed canvas and grow while rotated); vertical rotates
  its per-glyph placements in `layout/vertical.rs` (centroid via
  `vertical_layout_centroid`). The rotation matrix (`[cos -sin; sin cos]` on a
  positive angle, screen y-down) matches `tab.rs::default_overlay_quad_scene`
  (Ctrl+wheel), so a positive degree value turns text the same visual direction.
  It does NOT affect layout text, so it is excluded from
  `TextRenderShapeCompareParams` (like `anti_aliasing`).
- `TextRenderParams.raster_transform` (`Option<VectorMeshWarp>`) is a VECTOR
  mesh warp applied to glyph OUTLINES at the single rasterizer vertex seam
  (`vector::rasterize_outline_into`, `world = transform.apply(local)`), inserted
  AFTER per-glyph placement (path/formula/cell + group/inline rotation, already
  baked into the `GlyphTransform`) but BEFORE global rotation and rasterization.
  Normalization frame is the PRE-warp, PRE-global-rotation content AABB (`box`)
  the renderer already computes from the laid-out placements. `VectorMeshWarp`
  is a `cols x rows` lattice; `points_norm[i*cols+j]` is the warped normalized
  position of the node whose identity position is `(j/(cols-1), i/(rows-1))`.
  A point is bilinearly interpolated over `box` (points outside `[0,1]` clamp to
  the edge cell). Order at the seam is peel->warp->reapply:
  `world0 = R^-1(world about centroid C)` (peel global rotation), warp `world0`
  over `box`, `world = R(warped about C)` (reapply) — all via
  `vector::MeshWarpContext` (built once per render). `None`, an identity mesh,
  an invalid mesh, or a degenerate box take the BYTE-IDENTICAL fast path (`None`
  at the seam, no extra float work). Canvas bounds grow to the warped+rotated
  lattice-node extent (`MeshWarpContext::for_each_warped_bound_point` ->
  `PixelBounds::include_point`) so a strong outward warp never clips. Like
  `global_rotation_deg`/`anti_aliasing` it does not affect layout, so it is
  excluded from `TextRenderShapeCompareParams`. It is wired on EVERY layout mode:
  horizontal (Normal + `render_horizontal_rotated` in `pipeline.rs`), vertical
  (`layout/vertical.rs`), formula/on-path + `Shape` and custom raster/vector lines
  (`formula/render.rs`). Each path captures its OWN pre-global-rotation content
  box + rotation centroid BEFORE it applies the global rotation, then passes
  `Some(&ctx)` at its `rasterize_outline_into` seam and grows its bounds via
  `for_each_warped_bound_point`; the centroid/angle passed to
  `MeshWarpContext::new` are exactly the pivot/angle that path's
  `apply_global_rotation`/`rotate_placements_about_centroid`/vertical block
  rotation uses, so the peel/reapply is exact. Custom VECTOR lines normally emit a
  FIXED output canvas; a non-identity warp drops that fixed canvas (like a global
  rotation) and grows to the warped bounds so an outward warp never clips. The
  color-glyph BITMAP fallback on every path does NOT warp (only the monochrome
  outline seam does), matching the horizontal template. `raster_transform: None`
  or an identity mesh stays byte-identical on all paths (the pre-box capture is
  gated on `raster_transform.is_some()` and `MeshWarpContext::new` returns `None`
  for identity/invalid/degenerate). Production overlays wire it through the on-canvas
  VECTOR transform mode (`src/tabs/typing/tab/vector_transform.rs`).
- `TextRenderParams.line_placement_percent` (`[-100, 100]`, default 0) places
  each glyph PERPENDICULAR to the line/path at the vector level: `0` centers the
  glyph ink on the line, `+100` rests it ABOVE (сверху, ink bottom on the line),
  `-100` BELOW (снизу, ink top on the line). It is a SHOW-only feature of the two
  line-based modes `Formula` and `CustomVectorLines`. Because those render
  functions are shared with a HIDE sibling (`Shape` reuses the formula path,
  `CustomRasterLines` reuses the drawn-lines path), the pipeline router
  (`effective_line_placement_frac` in `pipeline.rs`) is the single gating source:
  it feeds the real value only for `Formula`/`CustomVectorLines` and `0.0` for
  every other mode, so a stale panel value cannot leak into a HIDE mode. The
  offset is applied by one shared helper (`formula::render::apply_line_placement`,
  `offset = line_frac * scaled_ink_height / 2` toward the top side; the top side
  is the negation of the `normal_offset_px` "down" normal `(-sin, cos)`). Both
  formula spots and the vector-lines `drawn_line_glyph_destination_center_raw`
  chokepoint use it. COMPAT: the vector-lines path places the glyph INK
  CENTER (not the baseline) on the line at 0%, so both line modes share one
  meaning of 0 = center; projects saved before the key default to 0.0 = center.
  Like `global_rotation_deg`, it does not affect layout text.
  REFERENCE BAND (`TextRenderParams.line_placement_reference`,
  `LinePlacementReference`, `CustomVectorLines` only): `GlyphHeight` is the legacy
  per-glyph anchoring above (each glyph centered by its OWN scaled ink height, so
  glyphs of differing height float to different offsets). `LineBox` instead anchors
  every glyph to one SHARED baseline: `drawn_line_glyph_destination_center_raw`
  computes a per-render `ascent_scaled` (primary font ascent at the line size ×
  base vertical stretch) and offsets each glyph by its own scaled top bearing so
  the baseline is constant — the line is a clean, just-curved string. `line_frac`
  then snaps the shared band top(-1)/center(0)/baseline(+1). Gated to
  `CustomVectorLines`; the panel defaults NEW text to `LineBox`, old projects load
  as `GlyphHeight`. Unit-tested by `line_box_reference_shares_one_baseline_across_glyph_heights`.
- Faux bold/italic (`TextRenderParams.faux_bold: Option<FauxBoldParams>`,
  `faux_italic_slant_deg: Option<f32>`): synthetic styles applied to the VECTOR
  outline instead of switching faces. They take effect ONLY together with the
  matching `force_bold`/`force_italic` flag; `force_* && faux present` keeps the
  Regular/upright face (no Bold/Italic font matching — `pipeline.rs`
  `base_attrs_real_bold_italic`), `force_*` without faux keeps the legacy
  real-face behavior byte-identically. Faux bold offsets the flattened outline
  by `d = thicken_percent/100 * glyph em` (`vector::offset_outline`:
  outer-vs-hole decided by the actual NonZero winding at a point just inside
  each ring — convention-free across TrueType/CFF; outer contours move
  outward, holes shrink only when `outward_only == false`, and a counter
  narrower than `2*d` collapses — the inverted/degenerate ring is dropped, not
  emitted; miter limit ~4 or round joins; offset self-intersections are
  absorbed by the NonZero fill) and grows each horizontal pen step by
  `2*d + expand_px` (`Fixed`/`Auto`; the trailing glyph's extra is folded into
  the logical `line_width_px` so alignment never overshoots; `Optical` instead
  measures the offset outlines and normalizes the true ink gaps — no pen
  growth, and `expand` does not apply). Faux italic is a baseline shear
  `x' = x - tan(slant) * y` between scale and rotation in `GlyphTransform`
  (advances unchanged), wired once through
  `glyph_blit::glyph_outline_transform` so all three draw paths get it.
  Per-variant caching: `OutlineKey`, `OpticalContourCache`, and the formula ink
  cache all carry the quantized faux bits (`vector::FauxOutlineParams`,
  1/64 px fixed point; `0` = plain, so plain geometry keys are untouched).
  Bounds are widened by the faux pads (`pipeline::faux_bounds_pads`; miter
  overhang up to `4*d`, shear overhang `|shear| * dy`), the vertical
  ink-height stacking grows via the padded ink profile and the column visual
  width includes the same bold+shear overhangs, and per-glyph resolution
  follows the inline-span-over-global rule (`pipeline::faux_style_for_glyph`).
  Inline grammar: `<b=thicken[,sharp|round][,out|both][,expand]>` /
  `<b=default>` / `<i=slant_deg>` (machine keys `b=`/`i=` take the same value
  payload); bare `<b>`/`<i>`/valueless machine keys keep the real faces. Faux
  spans EXPLICITLY reset attrs to `Weight::NORMAL`/`Style::Normal`
  (`inline_styles.rs`), so a global real bold/italic can never leak under the
  faux geometry.
- `TextRenderParams.extra_info` (`RenderExtraInfoRequest`) selects which optional
  "extra render info" items the renderer computes alongside the pixels — today the
  MEAN center (area centroid of the convex hull of all included glyphs'
  placement-box corners) and the MEDIAN center (per-axis median of their box
  centers), returned in `RenderedTextImage.extra` (`RenderedTextExtraInfo`, two
  `Option<[f32; 2]>` in FINAL-IMAGE pixels, fractional and possibly outside the
  image). The DEFAULT (nothing requested) is a true no-op with the byte-identical
  fast path and zero per-glyph sampling. Like `anti_aliasing`, it does NOT affect
  layout, so it is excluded from `TextRenderShapeCompareParams`; it is a per-render
  compute request and is NOT persisted in project JSON (codec/panel construction
  sites pass `default()`). Samples are taken at the VECTOR stage from the same
  placement box the draw pass uses (scaled/rotated, then mesh-warped through the
  same `MeshWarpContext::warp_world`); glyphs in a line's leading/trailing
  hanging-punctuation runs are excluded when `hanging_punctuation` is on (same
  edge-run semantics as `hanging_metrics_for_layout`). Offset consistency: the
  `finish` offset maps content->canvas, `raster::trim_rendered_image_to_alpha_bounds`
  shifts the centers by the crop origin, and `effects::apply_effects_pipeline`
  shifts them by the `content_origin_x/y` delta at its single exit (so effect
  order vs trim no longer matters). The contract now covers EVERY layout mode:
  horizontal Normal + `render_horizontal_rotated` (`pipeline.rs`), vertical
  (`layout/vertical.rs`), and formula/on-path + `Shape` + custom raster/vector
  lines (`formula/render.rs`). The shared accumulator API: build one
  `ExtraInfoAccumulator::new(params.extra_info)` per render, gate work on
  `is_active()`, call `add_glyph(corners, center, warpable)` per drawn glyph AFTER all
  placement transforms (post block/group/global rotation), apply the mesh warp once
  via `map_points(|p| ctx.warp_world(p))` when a warp is active, then
  `finish(x_offset, y_offset)` with that path's content->canvas offset and store the
  result into the image's `extra` before trim/effects. The `warpable` flag mirrors
  the draw branch: outline glyphs pass `true`, but color-glyph BITMAP-fallback glyphs
  pass `false` so `map_points` leaves their samples UNWARPED — matching the "bitmap
  fallback is not warped" pixel contract, so a bitmap-fallback sample contributes
  unwarped, aligned with its unwarped pixels. Each call site resolves the flag the
  same way the draw code does (`placement.outline.is_some()` on the horizontal
  Normal/rotated paths; the resolved outline `Option` on the vertical and
  formula/on-path + `Shape` + custom-line paths). The rotated draw paths build
  their per-glyph box via the shared `extra_info::rotated_box_samples` (scaled box
  half-extents rotated by the glyph's total rotation). Hanging-punctuation exclusion
  applies to the HORIZONTAL paths only (vertical/formula never read
  `hanging_punctuation`). Both formula render functions accumulate INSIDE their
  `_once` body, so the formula retry loop (`render_margin_pad` growth) naturally
  rebuilds a FRESH accumulator every iteration and the stored extras always match
  the accepted image.
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
- `hyphenation` provides the embedded per-language hyphenation dictionaries; the
  language is chosen by `ms_text_util::language::text_language`, and break
  boundary rules come from `ms_text_util::segmentation::rules` (group-dispatched).
- `image` provides RGBA/gray image containers and blur operations used by effects and
  drawn-line layout.
- `serde_json` is used only inside the effects parser; renderer callers pass effects as
  a JSON string through `TextRenderParams.effects_json`.

## Editing map
- To change caller-visible render parameters or result shape, start in `types.rs`, then
  update `mod.rs` smoke anchors and parent typing serialization/parsing.
- To change the mean/median extra-info math (hull centroid, median, degenerate
  fallbacks, offset mapping, or the shared `rotated_box_samples` corner geometry),
  edit `extra_info.rs`. To change WHERE it is sampled per layout mode, edit that
  mode's draw pass (`pipeline.rs` for horizontal Normal/rotated; `layout/vertical.rs`
  for vertical; `formula/render.rs` for formula/on-path + custom raster/vector
  lines). The trim/effects center-shift seams live in
  `raster::trim_rendered_image_to_alpha_bounds` and
  `effects::apply_effects_pipeline`.
- To change normal horizontal rendering, glyph scaling, kerning, hanging punctuation,
  line spacing, shape comparison, or routing, edit `pipeline.rs`. The normal
  horizontal path is SINGLE-PASS: `horizontal_run_layout` + `get_image` run once per
  glyph, collecting each into a `HorizontalGlyphPlacement`
  (`build_horizontal_placement`) reused for both the bounds box (via
  `include_scaled_rect_bounds` on the swash bitmap placement box) and the draw pass
  (`draw_horizontal_placement`); the inline-rotated path collects
  `RotatedGlyphPlacement` the same way. Horizontal monochrome glyphs rasterize from
  outlines via `glyph_blit::glyph_outline_transform`; the outline->world pivot lives
  in `glyph_blit.rs`, not here.
- Kerning-mode contract (`KerningMode`, `types.rs`): `Auto` (user label "Авто")
  applies font GPOS/`kern` pair kerning — the shaped cosmic-text positions plus
  manual tracking; it is the byte-identical successor of the historical `Metric`
  mode. `Fixed` (user label "Метрический") drops font pair kerning by stepping on
  each glyph's OWN nominal advance (`glyph_blit::nominal_glyph_advance_px`, read
  from the font `hmtx` table since cosmic-text bakes pair kerning into
  `LayoutGlyph.w`). `Optical` normalizes true ink-to-ink gaps; it is implemented
  but NOT offered in the panel UI (only ever set via a loaded/legacy value).
  Serialization: `Fixed`->`"fixed"`, `Auto`->`"auto"`, `Optical`->`"optical"`; the
  legacy token `"metric"` deserializes to `Auto` so old overlays render
  identically. On the vertical path the stacking is ink-height based (no font pair
  kerning), so `Fixed` and `Auto` coincide there; only `Optical` differs.
- Horizontal glyph pen positions live in `horizontal_run_layout`. `Auto` (and the
  `Optical` fallback when a run cannot be optically kerned) is byte-identical to
  the shaped positions plus manual tracking; `Fixed` uses the nominal own advance.
  `KerningMode::Optical` is IMPLEMENTED for the horizontal path:
  `optical_horizontal_run_layout` measures true ink-to-ink gaps between adjacent
  inked glyphs (outline contours placed through the same
  `glyph_blit::glyph_outline_transform` pivot as the draw pass) and normalizes
  them toward the run's median gap. It is gated entirely on `KerningMode::Optical`
  and shares the bounds/draw/rotated passes' `OutlineCache` plus a per-render ink
  contour cache. MVP limitation: optical pairs are considered only WITHIN a
  cosmic-text layout run (pairs straddling a run boundary keep the shaped
  advance). The pure numeric core (`median_of_gaps`, `optical_delta`,
  `optical_base_advance`) lives in the shared `optical.rs` module.
- The optical spacing math (`median_of_gaps`, `optical_delta`,
  `optical_base_advance`, the directional `optical_pair_gap` metric, the
  `OpticalContourCache` type, the simplify tolerance and min-gap floor) is
  axis-agnostic and lives ONLY in `optical.rs`. Both the horizontal
  (`pipeline.rs`) and vertical (`layout/vertical.rs`) paths reuse it; do not
  duplicate the formula or the metric. It is unit-tested in `optical.rs`.
  MEASUREMENT CONTRACT: the per-pair gap is the MINIMUM DIRECTIONAL projected
  whitespace along the advance axis — the closest facing points — NOT the Euclidean
  minimum distance (`min_placed_distance` is used only by the on-path/formula
  spacing, not here). It scans the pair's overlap band (horizontal: shared vertical
  band, gap = `cur_left - prev_right`; vertical: shared horizontal band,
  gap = `cur_top - prev_bottom`) and returns the SMALLEST per-scanline gap. That
  single min gap is both the target for median normalization (so the tightest
  points become uniform) and the collision floor. No band overlap -> infinite gap
  (not kerned). The directional projection removes the earlier sign-inversion on
  slanted/overhanging pairs (e.g. Cyrillic "ст"/"кс") that a diagonal min-distance
  produced.
  Optical spacing is exact on the measured layout but sub-pixel-approximate on the
  rendered pixels: the provisional measurement pen's `SubpixelBin` can differ from
  the accumulated draw pen by up to ~0.75px/axis. Since `delta` is now applied along
  the same axis the gap is measured on, the "final gap >= 0.5px" / "gaps converge to
  the median" contracts hold on the measured layout and are only sub-pixel-approximate
  on screen.
- To change faux bold/italic, the geometry (polyline offset, joins, shear) lives
  in `vector.rs` (`offset_outline`, `GlyphTransform.shear_x`,
  `FauxOutlineParams`); per-glyph resolution, advances, and bounds pads live in
  `pipeline.rs` (`FauxGlyphStyle`, `faux_style_for_glyph`,
  `faux_advance_extra_px_for_glyph`, `faux_bounds_pads`); the inline
  `<b=...>`/`<i=...>` grammar lives in `inline_styles.rs`.
- To change wrapping behavior, edit `wrap/`; keep measurement/scoring in
  `horizontal.rs`, dictionary/safety rules in `hyphenation.rs`, shape profiles in
  `shape.rs`, and vertical pre-layout in `vertical.rs`.
- To change vertical text positioning or optical spacing, edit `layout/vertical.rs`.
  `KerningMode::Optical` is IMPLEMENTED for the vertical path too: it measures the
  true top-to-bottom ink whitespace of adjacent inked glyphs in a column and
  normalizes it toward the column median, gated strictly on Optical (`Fixed`/`Auto`
  and every non-Optical mode stay byte-identical). It reuses `optical.rs` and shares the vertical
  render's `OutlineCache` + `OpticalContourCache`.
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
