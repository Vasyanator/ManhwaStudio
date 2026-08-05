# Module: src/tabs/typing/render_next/effects

## Purpose
This directory owns the JSON-driven effect system for the production typing renderer.
It keeps preprocess text mutations and post-raster image effects separate from base
layout, wrapping, and glyph rasterization.

## Architecture
`mod.rs` is the only entry point used by `render_next::pipeline`. It parses
`TextRenderParams.effects_json`, applies preprocess effects before inline-style
parsing, and applies post-effects after a `RenderedTextImage` has been produced.

The same post-effect routing is reused by `pipeline::apply_effects_to_image` for imported image
overlays: there is no text layout, the finished `RenderedTextImage` simply wraps the source RGBA
buffer, and `apply_effects_pipeline` runs unchanged. Effects must therefore keep operating purely
on the RGBA buffer (alpha contour / colors), never assuming the pixels came from rasterized text.

The data flow is:

1. `parse.rs` validates persisted JSON and converts aliases into typed effect params.
2. `apply_text_preprocess_effects` mutates source text before inline tag parsing.
3. The main renderer builds the base image.
4. `apply_effects_pipeline` routes typed post-effects to concrete image modules.
5. Image modules mutate the `RenderedTextImage` RGBA buffer in place, growing the
   canvas only when the effect needs extra margin.

`image_ops.rs` contains shared blur, distance-transform, dilation, sampling, blit, and
compositing helpers. Concrete effects should use it instead of duplicating image math.

Per-pixel/per-row effect passes are parallelized with `rayon` over rows (`par_chunks_mut`)
or independent pixels (`par_chunks_mut(4)`), always on the global rayon pool. Parallelized
passes write each output slot independently from read-only inputs, so they stay bit-identical
to a sequential pass; each converted effect has a golden test asserting exact parallel-vs-
sequential equality. `gaussian_blur_rgba_in_place` / `gaussian_blur_alpha_in_place` use a
separable two-pass Gaussian (horizontal over rows, vertical over rows) that replicates
`image::imageops::blur` (image 0.25): same kernel-size formula (`kernel_size_from_sigma`),
same normalized Gaussian weights, an `f32` intermediate horizontal pass (not re-quantized),
and replicate (clamp-to-edge) handling. It is kept within a 2/255 per-channel tolerance
(golden tests `separable_blur_matches_image_blur_within_tolerance` and
`separable_alpha_blur_matches_image_blur_within_tolerance`; verified sigma ∈
{0.35, 0.5, 1.0, 1.5, 2.4, 4.0, 8.0} on images with a hard 0->255 alpha edge, measured
max 0/255 for both the RGBA and alpha paths).

## Files and submodules
- `mod.rs`: effect pipeline entry points and routing from typed effect specs to modules.
- `parse.rs`: external effects JSON contract, stage parsing, aliases, defaults, and
  typed parameter structures.
- `stroke_shadow.rs`: alpha-contour stroke and shadow layers with optional blur/source
  color behavior.
- `blur.rs`: Gaussian blur and motion blur post-effects.
- `glow.rs`: contour glow (`glow_v1`/`glow_v2`), soft glow, and falloff. Soft glow dilates the
  source alpha by four independent per-side extents (`radius_px` plus a signed `expand_*` per
  side, clamped at 0) with either the rectangle (`Square`) or the per-quadrant ellipse (`Round`)
  element, subtracts the source to get the outline ring, then keeps that ring in `f32` through
  the Gaussian blur (`blur_radius_px`) and the `soft_glow_response_curve` bias/knee remap, with
  a single `u8` rounding at composite time. The curve is pinned at (0,0)/(1,1) and is EXACTLY
  the identity at bias 0, so persisted pre-curve effects render unchanged. Padding is per side
  (side extent + blur kernel half-width), so an asymmetric glow is not clipped and
  `content_origin_x/y` advance by the LEFT and TOP padding only — not by one symmetric scalar.
  No glow variant reduces its layer by the source alpha: `blend_source_text` composites the
  source over the finished glow, and that source-over is the ONE place the source coverage may
  enter the result. Pre-multiplying the glow by `1 - src_a` counts the coverage twice
  (`out_a = a + g(1-a)²` instead of `a + g(1-a)`) and punches a translucent notch into every
  antialiased rim, which reads as a light halo hugging the glyphs; `glow_v1`/`glow_v2` did that
  until it was removed, and the rim-notch tests pin the rule for all three variants. Cost asymmetry matters here: `Square` is `O(w*h)` while `Round`
  is `O(w*h*(up+down+1))`, so a 1000x600 source at radius 512 + expand 512 takes ~0.035 s square
  vs ~3.7 s round (~9.2 s with blur 256) in release, and that run cannot be aborted part-way —
  `apply_effects_pipeline` checks cancellation only BETWEEN effects.
  Both contour variants hold the glow-only alpha in `f32` end to end (no intermediate `u8` rounding) and
  post-blur it with a small sigma (`glow_smoothing_sigma`, ~1px, clamped `[0.8, 2.0]`) before
  compositing, so iso-distance plateaus no longer band; overlap and the color-alpha factor are
  applied after the blur with a single final `u8` round, and the canvas is padded by the glow
  reach plus the blur kernel half-width. `glow_v2` additionally seeds the EDT sub-pixel:
  partial coverage becomes a `d0*d0` initial cost (`d0 = (0.5 - a/255).max(0.0)`) instead of a
  binary 0/1 mask. `glow_v1` keeps its legacy integer disc-splat seeding (no sub-pixel EDT).
- `gradients.rs`: two-color and four-corner gradient fills. Two orthogonal parameters decide
  WHAT is repainted and WHERE the ramp is stretched. `fill_mode` (`all_opaque` /
  `specific_color`) plus `color_tolerance_percent` decide what: the tolerance is a share of
  the RGB cube diagonal (`sqrt(3)*255`) compared against the RGB distance to `target_color`,
  alpha excluded, so antialiased glyphs match at any coverage; 0 % is the byte-exact legacy
  match. `area_mode` decides where: `full_image` uses the bounding box of every
  non-transparent pixel (legacy), `affected_area` uses the bounding box of only the replaced
  pixels, so the ramp spans exactly the recolored shape. Both parameters default to the
  legacy behavior when absent, and under `all_opaque` the two area modes coincide.
- `reflect_shake.rs`: axis reflection and shake-trail composition.
- `dry_media.rs`: deterministic pencil/chalk texture erosion and dust/grain effects.
- `interference.rs`: deterministic interference (помехи/glitch) post-effect with four sub-kinds
  (`white_noise` per-pixel static, `digital` band displacement + RGB split, `rgb_split`
  chromatic aberration, `scanlines`). Every pass is a pure row gather from an immutable source
  snapshot (parallel == sequential); growing kinds pad the canvas (digital/scanlines: horizontal;
  rgb_split: all sides) and update `content_origin`. Shares the noise helpers in `image_ops.rs`.
- `image_ops.rs`: shared low-level image helpers used by multiple effects. Also owns the shared
  deterministic noise primitives (`hash_noise_signed` splitmix64 hash, `value_noise_signed`
  bilinear value noise, `smoothstep01`, `lerp_f32`, `i32_to_u64_wrapping`) used by both
  `dry_media` and `interference` so there is one tested noise implementation.
  Dilation has two production forms, both taking four per-side extents (`left`, `right`, `up`,
  `down`) and mirroring the structuring element as dilation requires (a `right` extent grows the
  shape to the right): `dilate_alpha_rect` is a separable sliding-window maximum (horizontal pass,
  then a vertical pass over a transposed copy) costing `O(width * height)` regardless of the
  extents, and `dilate_alpha_ellipse` walks each `dy` of the per-quadrant ellipse with the same
  row kernel, costing `O(width * height * (up + down + 1))` — seconds, not milliseconds, at
  radii in the hundreds, so callers must treat a large round dilation as a long uninterruptible
  pass. With equal extents the rectangle is byte-identical to the legacy iterated
  `dilate_alpha_max_filter3`, which is now a test-only reference. The EDT comes in
  two forms: `euclidean_distance_transform_to_mask` (binary 0/1 seeding) is a thin wrapper over
  `euclidean_distance_transform_with_costs`, the Felzenszwalb-Huttenlocher transform evaluated
  in `f32` over an arbitrary squared-distance cost field (a valid seed cost is in
  `[0, EDT_COST_INF)`; anything else is normalized to the non-seed sentinel). Blur has a `u8`
  form and an `f32` form (`gaussian_blur_alpha_f32_in_place`) for banding-free pre-composite
  smoothing; `gaussian_blur_kernel_radius` gives the padding a caller needs for the blur tail.

## Contracts and invariants
- Missing `effect_type` in JSON means a post-effect for backward compatibility.
- Preprocess effects run before inline-style parsing and may generate inline tags.
  They must not inspect font layout, glyphs, or raster output.
- Post-effects receive only a finished `RenderedTextImage`. They must not reach back
  into wrapping, layout, inline span remapping, project state, or UI state.
- Effect parsing must return clear `String` errors that name the stage, effect, or
  field that failed.
- RGBA buffers are unmultiplied and must remain `width * height * 4` bytes after every
  effect, including effects that pad or resize the image.
- An effect that draws a layer BEHIND the source and then composites the source back over it
  must give that layer its own alpha, unreduced by the source alpha. The straight-alpha
  source-over is the only place the source coverage may be applied; applying it twice leaves a
  translucent ring on every antialiased rim (see the glow bullet above).
- Empty images and zero-sized intermediate dimensions must be handled without panics.
- Long or multi-pass effects must honor the cancellation checks performed by
  `apply_effects_pipeline` before each effect.
- JSON aliases in `parse.rs` are part of the persisted renderer contract; update
  parser tests when changing them.

## Editing map
- To add or rename an effect, start in `parse.rs`, then route it in `mod.rs`, implement
  the image logic in a focused module, and add parser/image tests.
- To change interference (помехи/glitch) behavior or add a sub-kind, edit `interference.rs`
  (the sub-kind dispatch + `*_fill_row` kernels) and, for a new sub-kind, `InterferenceKind`
  and its parsing in `parse.rs`.
- To change soft-glow geometry (per-side extents, outline shape) or its blur response, edit
  `apply_soft_glow_effect` / `soft_glow_response_curve` in `glow.rs`, the dilation helpers in
  `image_ops.rs`, and `SoftGlowEffectParams` + `parse_soft_glow_effect_params` in `parse.rs`.
- To change which pixels a gradient repaints or which rectangle its ramp spans, edit
  `should_replace_gradient*` / `color_tolerance_threshold_sq` / `bounds_where` in
  `gradients.rs` and `parse_gradient_color_tolerance` / `parse_gradient_area_mode` +
  `GradientAreaMode` in `parse.rs`. The panel mirrors both fields in
  `src/tabs/typing/panel/{effect_parse,effect_cards}.rs`, so the JSON keys must stay in sync.
- To change the shared noise (grain/static) math, edit the noise helpers in `image_ops.rs`;
  both `dry_media` and `interference` depend on them.
- To change legacy JSON compatibility, edit `parse.rs` and update parent typing
  serialization only if the persisted contract changes.
- To change blur, dilation, distance transform, blitting, or shared sampling behavior,
  edit `image_ops.rs` and audit all effect modules that call the helper.
- To change text-mutating effects, update the preprocess path in `mod.rs` and keep the
  output compatible with `inline_styles.rs`.
- To change post-raster visual behavior, keep the work local to the concrete effect
  module unless shared image math actually changes.
