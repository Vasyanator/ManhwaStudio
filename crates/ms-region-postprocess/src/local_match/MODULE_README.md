# Module: crates/ms-region-postprocess/src/local_match

## Purpose
The local CIELAB colour match of the region post-processing: after the global mean/std match,
it pulls the low frequencies of the generated pixels inside the sample mask towards the
surroundings (lightness mean + contrast, a/b mean shift) at a user scale. A literal port of the
reference Python `match_color_local` (`modules/ai_backend/inpaint/region_imaging.py` at
`c11c4eb`), numpy-fallback helper paths included. Crate-private: the pipeline drives its three
steps and maps `LocalMatchError` into its own error.

## Architecture
The match is split where its inputs change, so the pipeline can cache each part:
1. `LocalSampling::new(reference, sample_mask)` — once per region: the 3 px margin dilation and
   the target (valid) and current (inside) sample sets; or "identity" for an empty mask / fewer
   than 256 valid pixels. The target's Lab is NOT cached (12 B/px for ~5 ms per scale change).
   `LocalSampling::new_band(band)` is the edge-band mode of a whole-region run (see below).
2. `LocalSampling::fields(reference, current, scale)` — per (current image, scale): the shared geometry
   (grid factor `floor(scale / 4)`, reflect-101 padding by whole cells plus the finest kernel
   reach), the centred Lab of each side (only where it samples), then `robust_local_moments`
   for both sides CONCURRENTLY (`rayon::join`): the TARGET (cascade of 5 scales `scale·2^i`,
   coarse levels on jittered point samples, ending with the global robust moments) and the
   CURRENT side (finest scale only). Each scale is an initial normalized convolution plus 2
   robust reweightings (`exp(-ΔE²/(2·12²))`). Result: the per-cell correction fields.
3. `LocalFields::apply(current, sample_mask, strength)` — the fields upsampled and applied to
   masked pixels only, blended by `strength`: the ONLY step `strength` enters.
`match_color_local` (tests only) is the one-shot composition the parity fixtures pin.

**Edge-band mode** (not in the reference): a whole-region run has no outside, so both images
are sampled on the SAME band pixels and step 2 builds one field: the robust local mode of
`Lab(reference) - Lab(current)` (uncentred: near 0 after the global stage), cascaded like the
target and ended by its robust global value (`offset_fields`). The correction is a pure offset
`[1, dL·t, da·t, db·t]`, `t = clamp(share / 0.25)` (`offset_correction`), and step 3 applies it
over the whole mask (the pipeline passes `mask`). A difference field, not two moment fields:
each side's robust mode would pick different band pixels.

## Files and submodules
- `mod.rs`: `LocalSampling` / `LocalFields` (the three steps), `match_color_local` (tests),
  `LocalMatchError`, geometry, the robust-moment cascade, the fields, the final remap.
- `grid.rs`: `Field` and the low-pass helpers (block mean, fused moment block mean, bilinear
  up, Gaussian, sigma-adaptive smooth, reflect fill, numpy pairwise sum, row scheduling).
- `lab.rs`: sRGB u8 <-> CIELAB (D65) f32 conversion tables.
- `jitter.rs`: splitmix64 and the per-block sample positions of the coarse levels.
- `tests.rs`: parity against the golden fixtures (`../../fixtures/`), contract tests,
  helper-equivalence tests, the ignored release benchmark.

## Contracts and invariants
- Only pixels inside the mask `apply` receives change; every other byte is `current`'s (tested
  exactly).
- Deterministic: f32 work with numpy 2 weak-scalar semantics and numpy's summation orders
  (`grid.rs` notes), f64 statistics summed sequentially; parallelism only over rows with
  per-row outputs (`grid::for_each_row`) and between the two independent sides, never a
  parallel float reduction.
- `fields()` depends on exactly `(reference, sample_mask or band, current, scale)`; `apply()` on the
  fields, `current`, `sample_mask` and `strength`. A cache keyed on less is wrong (the pipeline
  keys fields by global-match flag + scale, its `current` being fixed per flag).
- Deliberate deviation from the reference: the coarse-level jitter uses splitmix64 instead of
  numpy's PCG64 (`jitter.rs`); the parity fixtures are generated with the same rule.
- Not in the reference: `scale_px > MAX_SCALE_PX` (1024) is refused, because the padding grows
  by about `4·scale` px per side.
- Pixels with zero sample weight are never converted to Lab (they hold 0): every use is
  multiplied by that weight or skipped, and a `±0` addend leaves sums starting at `+0`
  unchanged. Keep that argument valid when adding a consumer of the padded Lab fields.
- GUI-free, no I/O, no logging; the caller logs and times the run.

## Editing map
- Constants, the three steps and the overall flow: `mod.rs`.
- A numpy-order or border question in a low-pass helper: `grid.rs` (the equivalence tests in
  `tests.rs` pin the fused / tap-major fast paths to their direct forms).
- Colour conversion: `lab.rs`. Sampling pattern: `jitter.rs` (change it only together with the
  fixture generator `tools/make_region_postprocess_fixtures.py`).
