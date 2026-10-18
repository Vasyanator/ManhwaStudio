# Module: crates/ms-region-postprocess

## Purpose
The one owner of the FLUX.2 / FLUX.3 klein region post-processing: the persisted post settings
(defaults, ranges, the whole-region rule), the cv2-exact elliptical mask dilation, the global and
local colour match, the inward feather, the rounded composite, the cached re-derivation of a
result when a setting changes, and the body of the «Постобработка» settings section. The Python
backend returns the RAW model output; everything here runs in Rust, so a post setting re-applies
to a shown result without a new run.

The crate also owns the FLUX.2 klein LATENT mask: the engine builds it BEFORE a run with
`dilate_ellipse(mask, mask_dilate_px)` and sends it to the backend, and the same mask is the
colour-match `sample_mask` of the result. The name says "postprocess"; the dilation is shared on
purpose so the latent mask and the colour-match margins use one kernel rule.

Layer: the `ms-studio-tab` line of `ARCHITECTURE.md` §2 (above `ms-raster`, `ms-widgets`,
`ms-i18n`). Consumers: `ms-tab-cleaning` (the region-edit host and the FLUX engines); the cloud
editor (`ms-ai-api::image_edit`) may use it later. It never depends on a tab crate, `ms-models`
or `ms-ai-api`.

## Architecture
```text
RegionInputs { base, reference?, generated, mask, sample_mask? }   (RGB8 / L8, row-major)
  -> RegionPostprocessor::new (validates every buffer)
  -> render(EffectiveSettings) / render_masked_into(settings, out, write):
       rayon::join(
         alpha   = feather(inside_distance(mask), feather_px)          [distance once, alpha per width]
         global  = global_match(generated vs reference, ring)           [once]
         local   = sampling(sample_mask)                                [once, built beside global]
                   -> fields(global or generated, scale)                [per (global on/off, scale), few kept]
                   -> apply(fields, strength)                           [last (global, scale, strength)]
       )
       result  = composite(base, local, mask, alpha)  (outside mask = base bytes; `render_masked_into`
                 writes only the masked pixels of a caller's buffer, e.g. the host's Color32 image)
PostprocessSettings --effective(whole_region)--> EffectiveSettings
draw_postprocess_body(ui, &mut PostprocessSettings, id_salt) -> changed
```
`reference` defaults to `base` (it differs when the region SENT to the model carried overlay
marks: the colour match needs the image the model round-tripped, the composite the unmarked one);
`sample_mask` defaults to `mask` (FLUX.2: the dilated latent mask).

## Files and submodules
- `fixtures/`: golden parity data generated from the Python reference (`fixtures/README.md`).
- `src/lib.rs`: crate root, re-exports, `PostprocessError`, shape helpers.
- `src/settings.rs`: `PostprocessSettings` (persisted keys), `EffectiveSettings`, ranges and defaults.
- `src/pipeline.rs`: `RegionInputs`, `RegionPostprocessor` (stage caches, `render`,
  `render_masked_into`), `postprocess_region`; the cache keys and the field-cache bounds.
- `src/mask.rs`: `dilate_ellipse` (public, radius capped at `MAX_DILATE_RADIUS_PX`; also used by
  `local_match/` for its 3 px margin).
- `src/global_match.rs`: ring mean/std match; numpy-float32-exact statistics; `round_to_u8`.
- `src/local_match/`: robust local CIELAB match (cascade, coarse grid, jitter), split into
  `LocalSampling` (once) / `fields` (per input + scale) / `apply` (per strength). See its readme.
- `src/feather.rs`: inside distance (exact EDT, region border = contour) and the smoothstep alpha.
- `src/composite.rs`: the rounded blend, outside the mask verbatim; `composite_masked_into` writes
  it into a caller's pixel buffer, row-parallel.
- `src/ui.rs`: `draw_postprocess_body`, `section_title`.
- `src/fixture_tests.rs` (`cfg(test)`): golden parity against `fixtures/` (see `fixtures/README.md`).

## Contracts and invariants
- **The mask is a PERMISSION TO CHANGE.** Outside `mask` the result is `base` byte for byte; the
  feather is applied INWARDS so the weight is 0 there anyway. Never replace it with a plain
  Gaussian feather. Tested on every fixture case.
- **Field names of `PostprocessSettings` are persisted keys** (`mask_feather_px`,
  `color_match`, `color_match_local_pct`, `color_match_scale_px`), embedded flattened in the
  engines' settings; never rename. Defaults 12 / true / 60 / 24; ranges 0..=32, 0..=100, 8..=96.
- **Whole-region rule** (`effective`): with a solid mask there is no ring to measure, so both
  colour matches are off (a fit on the changed pixels would undo the edit); the feather stays, it
  joins the region to the page. The local match runs only under the global one.
- **Global match**: per-channel RGB mean/std over the pixels outside `sample_mask` grown by 6 px,
  so a text outline or halo at the contour does not bias it; a ring too thin for the margin falls
  back to the undilated mask, one under 256 samples skips. It fixes the VAE's tone shift of the
  whole window (a gain + offset per channel is all that shift needs) and rewrites every pixel.
- **Local match**: what the model PAINTED drifts towards the scene average at the mask centre,
  which no global curve fixes. In CIELAB at scale `s` (the Gaussian sigma of the finest level):
  lightness mean and contrast (gain clipped to 0.8..1.4), a/b shift only, blended at `strength`.
  The generated side's statistics come from inside the mask only; the target's from the original
  outside the mask (+3 px), carried inwards by a cascade `s..16s` ended by the original's global
  moments. Both sides are robust local modes (two `exp(-ΔE²/2·12²)` reweighting passes per scale);
  where a side's similar-pixel share is below 0.25 the correction fades to identity. Coarser
  levels use one jittered sample per `2^i` block. Default strength 60 %: full strength turns
  dark-area noise into specks.
- **The region border is a mask contour**: the distance is measured on the mask padded by a
  1 px unmasked ring, because pixels past the region edge belong to a page this edit may not
  change. Without it a mask painted up to the edge meets the page with a hard step and a
  whole-region mask gets no feather.
- **`mask_feather_px` is the ramp WIDTH**: smoothstep of the distance, 0 on the contour, 1 at
  exactly `feather_px` inside; a thinner mask compresses the ramp to its own half-width instead of
  losing the edit.
- **Every u8 result rounds half to even** (numpy `rint`), never truncates: a truncating blend
  darkens every partially blended pixel by up to one level, a mask-shaped patch with a hard
  contour. Blending a region with itself is an exact identity.
- **Deterministic**: equal inputs give equal bytes, whatever the thread count and whatever was
  rendered before on the same processor (`alternating_settings_equal_fresh_renders`). Parallel
  passes are per pixel or per row; whole statistics run concurrently; the global match's large
  pairwise sums split only along numpy's own halving tree. Never a split float reduction in any
  other order.
- **Caches** (`pipeline.rs`): a cache key names every input of its stage. Fields: global flag +
  scale (reference and masks are fixed per processor); remap: fields + strength bits. At most
  `FIELDS_CACHE_LEN` field sets within `FIELDS_CACHE_BYTES` are kept, newest always.
- **Deliberate deviations from the Python reference** (region_imaging.py at `c11c4eb`): the
  feather distance is the EXACT Euclidean transform (`ms_raster::squared_distance_to_mask`)
  instead of cv2's 5x5 chamfer; the local match's block jitter is a `splitmix64` hash of
  `(level, block_row, block_col)` instead of numpy's PCG64. The PATCHED fixtures encode both.
- **Errors**: buffers are validated by `RegionPostprocessor::new` (`EmptyRegion`,
  `SizeOverflow`, `ShapeMismatch`); `render` refuses a non-finite / out-of-range strength and a
  zero scale; `dilate_ellipse` refuses a radius above `MAX_DILATE_RADIUS_PX` (1024, so a hostile
  radius never allocates or loops unbounded). No panics on caller data, no logging (callers log).
- **Threading**: CPU work on the global rayon pool (the feather beside the colour chain, the two
  local sides beside each other, row-parallel passes); call `render` off the GUI thread.
  `ms-raster`'s distance transform stays sequential (a std-only level-0 crate); it is hidden by
  the `rayon::join`. `draw_postprocess_body` is cheap.
- **Memory** per processor, beside the inputs: global + remapped image (3 B/px each), distance
  (4 B/px), alpha (1 B/px), the local sampling (2 B/px sample sets) and the kept
  field sets (coarse grids, `FIELDS_CACHE_BYTES` at most).
- **UI**: body only, no chrome; the host draws the section header (collapsed by default) with
  `section_title()` and a stable `id_salt`. No literal strings except the `%` unit symbol.

## Parity
`fixture_tests.rs` asserts: ellipse kernels r = 1..=64 and dilations EXACT; global stage and
feather alpha EXACT (the global statistics mirror numpy's float32 pairwise sums); the composite of the reference's own
stage outputs exact; end to end `max |Δ| <= 2`, mean inside the mask <= 0.3; drift against
today's shipped (UNPATCHED) output only `p99 <= 6` (printed with `--nocapture`). Regenerate the
fixtures with `venv/bin/python -I tools/make_region_postprocess_fixtures.py`.

## Editing map
All paths below are under `src/`.
- Defaults or ranges: `settings.rs` (the UI dials read them).
- A new stage or a cache key: `pipeline.rs` (extend `alternating_settings_equal_fresh_renders`).
- The ring statistics: `global_match.rs`; the local CIELAB match: `local_match/`.
- The edge ramp: `feather.rs`; the blend: `composite.rs`.
- Labels and hints: `ui.rs` + `cleaning.region_postprocess.*` in `ms-i18n` `en.json` / `ru.json`.
