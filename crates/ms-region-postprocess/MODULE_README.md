# Module: crates/ms-region-postprocess

## Purpose
The one owner of the FLUX.2 / FLUX.3 klein region post-processing: the persisted post settings
(defaults, ranges, the effective rule), the cv2-exact elliptical mask dilation, the global and
local colour match (against the ring around a painted mask, or the edge band of a whole-region
run), the inward feather, the rounded composite, the cached re-derivation of a
result when a setting changes, and the body of the «Постобработка» settings section. The Python
backend returns the RAW model output; everything here runs in Rust, so a post setting re-applies
to a shown result without a new run.

The crate also owns the FLUX.2 klein LATENT mask: the engine builds it BEFORE a run with
`dilate_ellipse(mask, mask_dilate_px)` and sends it to the backend, and the same mask is the
colour-match `sample_mask` of the result. The name says "postprocess"; the dilation is shared on
purpose so the latent mask and the colour-match margins use one kernel rule.

It is also the project's one owner of the sRGB <-> CIELAB (D65) conversion (`lab`, public:
`LabTables`, `encode_srgb`): the local match and the cleaning tab's gradient fill use the same
tables. A new CIELAB consumer calls it instead of writing its own matrices.

Look-alike rules that are deliberately NOT routed here (each documents why at its site):
- the patch tool's coverage ramp (`ms-tools` `patch::membrane`): city-block depth, image border
  not a contour, outer ring at 0 — the patch blend is pinned on that ramp, not on this feather;
- the text renderer's `dilate_alpha_ellipse` (`ms-text-render` effects): grey-level, four
  per-side semi-axes, exact lattice inequality — a styling element, not cv2's;
- the cloud editor's `composite_feathered` (`ms-ai-api` `image_edit`): an OUTWARD feather
  (square dilation + box blur) — a provider re-renders the whole frame, so the paint must be
  covered completely and the soft edge falls on source pixels.

Layer: the `ms-studio-tab` line of `ARCHITECTURE.md` §2 (above `ms-raster`, `ms-widgets`,
`ms-i18n`). Consumers: `ms-tab-cleaning` (the region-edit host and the FLUX engines); the cloud
editor (`ms-ai-api::image_edit`) may use it later. It never depends on a tab crate, `ms-models`
or `ms-ai-api`.

## Architecture
```text
RegionInputs { base, reference?, generated, mask, sample_mask?, colour_reference }   (RGB8 / L8, row-major)
  -> RegionPostprocessor::new (validates every buffer)
  -> render(EffectiveSettings) / render_masked_into(settings, out, write):
       rayon::join(
         alpha   = feather(inside_distance(mask), feather_px)          [distance once, alpha per width]
         global  = global_match(generated vs reference, ring | band)    [once]
         local   = sampling(sample_mask | band)                         [once, built beside global]
                   -> fields(global or generated, scale)                [per (global on/off, scale), few kept]
                   -> apply(fields, strength)                           [last (global, scale, strength)]
       )
       result  = composite(base, local, mask, alpha)  (outside mask = base bytes; `render_masked_into`
                 writes only the masked pixels of a caller's buffer, e.g. the host's Color32 image)
PostprocessSettings --effective()--> EffectiveSettings
draw_postprocess_body(ui, &mut PostprocessSettings, id_salt) -> changed
```
`reference` defaults to `base` (it differs when the region SENT to the model carried overlay
marks: the colour match needs the image the model round-tripped, the composite the unmarked one);
`sample_mask` defaults to `mask` (FLUX.2: the dilated latent mask). `colour_reference` is the
run's mode (`Ring` for a painted mask, `EdgeBand` for a whole-region run), fixed per result.

## Files and submodules
- `fixtures/`: golden parity data generated from the Python reference (`fixtures/README.md`).
- `src/lib.rs`: crate root, re-exports, `PostprocessError`, shape helpers.
- `src/settings.rs`: `PostprocessSettings` (persisted keys), `EffectiveSettings`, ranges and defaults.
- `src/pipeline.rs`: `RegionInputs`, `ColourReference`, `RegionPostprocessor` (stage caches, `render`,
  `render_masked_into`), `postprocess_region`; the cache keys and the field-cache bounds.
- `src/lab.rs`: `LabTables` (sRGB u8 <-> CIELAB f32, numpy-exact) and `encode_srgb`; public.
- `src/mask.rs`: `dilate_ellipse` (public, radius capped at `MAX_DILATE_RADIUS_PX`; also used by
  `local_match/` for its 3 px margin).
- `src/global_match.rs`: mean/std match — ring selection (`match_color_global`) or edge band
  (`match_color_band`) over one fit; numpy-float32-exact statistics; `round_to_u8`.
- `src/edge_band.rs`: `EdgeBand`, the reference frame of a whole-region run (`EDGE_BAND_PX`,
  `EDGE_BAND_SIDE_DIVISOR`).
- `src/local_match/`: robust local CIELAB match (cascade, coarse grid, jitter), split into
  `LocalSampling` (once) / `fields` (per input + scale) / `apply` (per strength). See its readme.
- `src/feather.rs`: inside distance (exact EDT, region border = contour) and the smoothstep alpha.
- `src/composite.rs`: the rounded blend, outside the mask verbatim; `composite_masked_into` writes
  it into a caller's pixel buffer, row-parallel.
- `src/ui.rs`: `draw_postprocess_body`, `section_title`.
- `src/fixture_tests.rs` (`cfg(test)`): golden parity against `fixtures/` (see `fixtures/README.md`).
- `src/band_mode_tests.rs` (`cfg(test)`): Rust-only properties of the edge-band mode (no Python
  reference) and its ignored 2048² release benchmark.

## Contracts and invariants
- **The mask is a PERMISSION TO CHANGE.** Outside `mask` the result is `base` byte for byte; the
  feather is applied INWARDS so the weight is 0 there anyway. Never replace it with a plain
  Gaussian feather. Tested on every fixture case.
- **Field names of `PostprocessSettings` are persisted keys** (`mask_feather_px`,
  `color_match`, `color_match_local_pct`, `color_match_scale_px`), embedded flattened in the
  engines' settings; never rename. Defaults 12 / true / 60 / 24; ranges 0..=32, 0..=100, 8..=96.
- **Effective rule** (`effective`): the local match runs only under the global one; the run's mode
  does not enter it (it picks the reference instead, `ColourReference`).
- **Edge-band mode** (`ColourReference::EdgeBand`, a whole-region run): there is no ring outside a
  solid mask, and a fit inside it would undo the edit, so the reference is the region's outer
  frame — `EDGE_BAND_PX` = 12 px, at most a quarter of the shorter side
  (`EDGE_BAND_SIDE_DIVISOR`), where the picture still has to meet the page. Thin on purpose: a
  model drift growing from the border biases a wide band. Global: the same fit on the band, NO
  margin (no contour, no halo). Local: the robust local mode of `Lab(original) - Lab(current)` on
  the band, cascaded inwards like the ring's target, applied as an OFFSET (gain 1) over the whole
  mask at the local strength and scale (strength 0 = global only). `sample_mask` is ignored. Under
  256 band samples both stages skip. An edit meant to change the border is pulled back there:
  the user turns the colour match off for it. No Python reference: `band_mode_tests.rs`.
- **Global match** (ring mode): per-channel RGB mean/std over the pixels outside `sample_mask` grown by 6 px,
  so a text outline or halo at the contour does not bias it; a ring too thin for the margin falls
  back to the undilated mask, one under 256 samples skips. It fixes the VAE's tone shift of the
  whole window (a gain + offset per channel is all that shift needs) and rewrites every pixel.
- **Local match** (ring mode): what the model PAINTED drifts towards the scene average at the mask centre,
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
  scale (reference, masks and the reference mode are fixed per processor); remap: fields +
  strength bits — in both modes a strength change is one remap. At most
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
  (4 B/px), alpha (1 B/px), the local sampling (2 B/px ring sample sets, 1 B/px band) and the kept
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
- The ring / band statistics: `global_match.rs`; the band geometry: `edge_band.rs`; the local
  CIELAB match (both modes): `local_match/`; the colour conversion itself: `lab.rs` (pinned by
  the parity fixtures AND by the gradient fill's characterization tests in `ms-tab-cleaning`).
- The edge ramp: `feather.rs`; the blend: `composite.rs`.
- Labels and hints: `ui.rs` + `cleaning.region_postprocess.*` in `ms-i18n` `en.json` / `ru.json`.
